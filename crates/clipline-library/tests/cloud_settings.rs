use clipline_library::cloud::cache::{AccountPublicationGuard, CloudCacheError};
use clipline_library::cloud::cache_identity::CloudCacheNamespace;
use clipline_library::cloud::ports::{CloudAccountPort, CloudProfilePatch};
use clipline_library::cloud::settings::{
    cloud_cache_account_fence_from_service_account, cloud_cache_account_fence_from_snapshot,
    cloud_service_account_from_snapshot, SettingsAccountPublicationGuard, SettingsCloudAccountPort,
};
use clipline_library::{
    CloudAccountGeneration, CloudAccountKey, MAX_CATALOG_STRING_BYTES, MAX_CLOUD_INDEX_ROWS,
};
use clipline_settings::{
    AccountGeneration, AppSettings, CloudAccountIdentity, CloudUploadRecord, SettingsChange,
    SettingsProfile, SettingsRevision, SettingsSnapshot, SettingsStore, SettingsTransaction,
};
use clipline_test_utils::TestDir;

fn cloud_record(local_id: &str, client_id: Option<&str>, path: &str) -> CloudUploadRecord {
    CloudUploadRecord {
        local_clip_id: local_id.into(),
        client_clip_id: client_id.map(str::to_owned),
        upload_generation: Some(1),
        path: path.into(),
        remote_clip_id: None,
        remote_url: None,
        visibility: "private".into(),
        upload_status: "queued".into(),
        error: None,
        updated_at_unix: 1,
    }
}

fn connected_settings() -> AppSettings {
    let mut settings = AppSettings::default();
    settings.cloud.host_url = "https://cloud.example".into();
    settings.cloud.public_url = Some("https://clips.example".into());
    settings.cloud.connected_user_id = Some("user-1".into());
    settings.cloud.connected_username = Some("clipper".into());
    settings.cloud.connected_display_name = Some("Clipper".into());
    settings.cloud.credential_target = Some(clipline_settings::cloud::cloud_credential_target(
        &settings.cloud.host_url,
        "user-1",
    ));
    settings.cloud.default_visibility = "unlisted".into();
    settings.cloud.delete_local_after_upload = true;
    settings.cloud.auto_upload_rules = true;
    settings
}

fn snapshot(document: AppSettings) -> SettingsSnapshot {
    SettingsSnapshot {
        account: CloudAccountIdentity::from_settings(&document.cloud),
        document,
        revision: SettingsRevision::INITIAL,
        account_generation: AccountGeneration::INITIAL,
    }
}

fn connected_store(name: &str) -> (TestDir, SettingsStore) {
    let directory = TestDir::new("clipline-cloud-settings", name);
    let profile = SettingsProfile::isolated(directory.path());
    let mut settings = connected_settings();
    settings.media_dir = profile.default_media_dir().display().to_string();
    settings.save_to(profile.settings_path()).unwrap();
    (directory, SettingsStore::open(profile))
}

#[test]
fn snapshot_conversion_preserves_preferences_and_uses_client_ids_for_local_paths() {
    let mut settings = connected_settings();
    settings.cloud.uploads.insert(
        "durable-local".into(),
        cloud_record("durable-local", Some("server-client"), r"C:\Media\one.mp4"),
    );
    settings.cloud.uploads.insert(
        "legacy-local".into(),
        cloud_record("legacy-local", None, r"C:\Media\legacy.mp4"),
    );

    let account = cloud_service_account_from_snapshot(&snapshot(settings)).unwrap();

    assert_eq!(account.snapshot.generation.get(), 1);
    assert!(account.snapshot.connected);
    assert_eq!(account.snapshot.host_url, "https://cloud.example");
    assert_eq!(
        account.snapshot.public_url.as_deref(),
        Some("https://clips.example")
    );
    assert_eq!(account.snapshot.username.as_deref(), Some("clipper"));
    assert_eq!(account.snapshot.display_name.as_deref(), Some("Clipper"));
    assert_eq!(account.snapshot.user_id.as_deref(), Some("user-1"));
    assert_eq!(account.snapshot.default_visibility, "unlisted");
    assert!(account.snapshot.delete_local_after_upload);
    assert!(account.snapshot.auto_upload_rules);
    assert_eq!(
        account
            .local_paths_by_clip_id
            .get("server-client")
            .map(String::as_str),
        Some(r"C:\Media\one.mp4")
    );
    assert!(!account.local_paths_by_clip_id.contains_key("durable-local"));
    assert_eq!(
        account
            .local_paths_by_clip_id
            .get("legacy-local")
            .map(String::as_str),
        Some(r"C:\Media\legacy.mp4")
    );
}

#[test]
fn snapshot_conversion_rejects_ambiguous_client_id_paths() {
    let mut settings = connected_settings();
    settings.cloud.uploads.insert(
        "local-a".into(),
        cloud_record("local-a", Some("same-client"), r"C:\Media\a.mp4"),
    );
    settings.cloud.uploads.insert(
        "local-b".into(),
        cloud_record("local-b", Some("same-client"), r"C:\Media\b.mp4"),
    );

    let error = cloud_service_account_from_snapshot(&snapshot(settings)).unwrap_err();

    assert!(error.to_string().contains("same-client"));
    assert!(error.to_string().contains("multiple local paths"));
}

#[test]
fn snapshot_conversion_accepts_the_row_cap_and_rejects_one_over_before_projection() {
    let mut settings = connected_settings();
    for index in 0..MAX_CLOUD_INDEX_ROWS {
        let id = format!("local-{index:05}");
        settings.cloud.uploads.insert(
            id.clone(),
            cloud_record(&id, None, &format!(r"C:\Media\{index:05}.mp4")),
        );
    }
    let account = cloud_service_account_from_snapshot(&snapshot(settings.clone())).unwrap();
    assert_eq!(account.local_paths_by_clip_id.len(), MAX_CLOUD_INDEX_ROWS);

    settings.cloud.uploads.insert(
        "one-over".into(),
        cloud_record("one-over", None, r"C:\Media\one-over.mp4"),
    );
    let error = cloud_service_account_from_snapshot(&snapshot(settings)).unwrap_err();
    assert!(error.to_string().contains("10001 upload records"));
    assert!(error
        .to_string()
        .contains(&format!("maximum is {MAX_CLOUD_INDEX_ROWS}")));
}

#[test]
fn snapshot_conversion_checks_client_id_and_path_before_cloning() {
    let mut settings = connected_settings();
    let exact_id = "i".repeat(MAX_CATALOG_STRING_BYTES);
    let exact_path = "p".repeat(MAX_CATALOG_STRING_BYTES);
    settings.cloud.uploads.insert(
        "slot".into(),
        cloud_record("slot", Some(&exact_id), &exact_path),
    );
    let exact = cloud_service_account_from_snapshot(&snapshot(settings.clone())).unwrap();
    assert_eq!(
        exact.local_paths_by_clip_id.get(&exact_id),
        Some(&exact_path)
    );

    settings
        .cloud
        .uploads
        .get_mut("slot")
        .unwrap()
        .client_clip_id = Some("i".repeat(MAX_CATALOG_STRING_BYTES + 1));
    let oversized_id =
        cloud_service_account_from_snapshot(&snapshot(settings.clone())).unwrap_err();
    assert!(oversized_id.to_string().contains("cloud client clip id"));

    let record = settings.cloud.uploads.get_mut("slot").unwrap();
    record.client_clip_id = Some("client".into());
    record.path = "p".repeat(MAX_CATALOG_STRING_BYTES + 1);
    let oversized_path = cloud_service_account_from_snapshot(&snapshot(settings)).unwrap_err();
    assert!(oversized_path.to_string().contains("cloud local path"));
}

#[test]
fn snapshot_conversion_preflights_account_strings_before_cloning() {
    let mut settings = connected_settings();
    settings.cloud.public_url = Some("u".repeat(MAX_CATALOG_STRING_BYTES + 1));

    let error = cloud_service_account_from_snapshot(&snapshot(settings)).unwrap_err();

    assert!(error.to_string().contains("cloud public URL"));
    assert!(error
        .to_string()
        .contains(&format!("maximum is {MAX_CATALOG_STRING_BYTES}")));
}

#[test]
fn settings_port_applies_only_an_exact_profile_and_rejects_replacement_login() {
    let (directory, store) = connected_store("profile-fence");
    let port = SettingsCloudAccountPort::new(store.clone());
    let current = port.snapshot().unwrap();
    let before_unrelated = store.snapshot().unwrap();
    let unrelated_media_root = directory.path().join("other-media").display().to_string();
    store
        .transact(SettingsTransaction {
            expected_revision: before_unrelated.revision,
            expected_account_generation: before_unrelated.account_generation,
            change: SettingsChange::SetMediaRoot(unrelated_media_root.clone()),
        })
        .unwrap();
    let updated = port
        .apply_profile(
            &current.snapshot.account_key,
            current.snapshot.generation,
            CloudProfilePatch {
                user_id: "user-1".into(),
                username: "updated-name".into(),
                display_name: Some("Updated Name".into()),
            },
        )
        .unwrap();
    assert_eq!(updated.snapshot.username.as_deref(), Some("updated-name"));
    assert_eq!(
        updated.snapshot.display_name.as_deref(),
        Some("Updated Name")
    );
    assert_eq!(
        store.snapshot().unwrap().document.media_dir,
        unrelated_media_root
    );
    assert_eq!(
        store
            .snapshot()
            .unwrap()
            .document
            .cloud
            .connected_username
            .as_deref(),
        Some("updated-name")
    );

    let before_login = store.snapshot().unwrap();
    let mut replacement = before_login.document.cloud;
    replacement.connected_user_id = Some("user-2".into());
    replacement.credential_target = Some(clipline_settings::cloud::cloud_credential_target(
        &replacement.host_url,
        "user-2",
    ));
    store
        .transact(SettingsTransaction {
            expected_revision: before_login.revision,
            expected_account_generation: before_login.account_generation,
            change: SettingsChange::ReplaceCloudProfile(replacement),
        })
        .unwrap();

    let error = port
        .apply_profile(
            &current.snapshot.account_key,
            current.snapshot.generation,
            CloudProfilePatch {
                user_id: "user-1".into(),
                username: "stale".into(),
                display_name: None,
            },
        )
        .unwrap_err();
    assert!(error.is_account_changed());
}

#[test]
fn settings_port_fences_account_key_generation_and_profile_user_independently() {
    let (_directory, store) = connected_store("profile-exact-fences");
    let port = SettingsCloudAccountPort::new(store.clone());
    let current = port.snapshot().unwrap();
    let durable_before = store.snapshot().unwrap();

    let cases = [
        (
            CloudAccountKey::new("wrong-account").unwrap(),
            current.snapshot.generation,
            "user-1",
        ),
        (
            current.snapshot.account_key.clone(),
            CloudAccountGeneration::new(current.snapshot.generation.get() + 1),
            "user-1",
        ),
        (
            current.snapshot.account_key.clone(),
            current.snapshot.generation,
            "different-user",
        ),
    ];
    for (key, generation, user_id) in cases {
        let error = port
            .apply_profile(
                &key,
                generation,
                CloudProfilePatch {
                    user_id: user_id.into(),
                    username: "must-not-persist".into(),
                    display_name: None,
                },
            )
            .unwrap_err();
        assert!(error.is_account_changed());
        assert_eq!(store.snapshot().unwrap(), durable_before);
    }
}

#[test]
fn settings_port_prioritizes_each_stale_owner_fence_over_an_invalid_patch() {
    let (_directory, store) = connected_store("profile-stale-invalid-precedence");
    let port = SettingsCloudAccountPort::new(store.clone());
    let current = port.snapshot().unwrap();
    let durable_before = store.snapshot().unwrap();
    let oversized = "x".repeat(MAX_CATALOG_STRING_BYTES + 1);
    let cases = [
        (
            CloudAccountKey::new("wrong-account").unwrap(),
            current.snapshot.generation,
            "user-1",
        ),
        (
            current.snapshot.account_key.clone(),
            CloudAccountGeneration::new(current.snapshot.generation.get() + 1),
            "user-1",
        ),
        (
            current.snapshot.account_key.clone(),
            current.snapshot.generation,
            "different-user",
        ),
    ];

    for (key, generation, user_id) in cases {
        let error = port
            .apply_profile(
                &key,
                generation,
                CloudProfilePatch {
                    user_id: user_id.into(),
                    username: oversized.clone(),
                    display_name: None,
                },
            )
            .unwrap_err();
        assert!(error.is_account_changed());
        assert_eq!(store.snapshot().unwrap(), durable_before);
    }
}

#[test]
fn settings_port_rejects_empty_or_oversized_profile_content_without_mutation() {
    let (_directory, store) = connected_store("profile-field-bounds");
    let port = SettingsCloudAccountPort::new(store.clone());
    let current = port.snapshot().unwrap();
    let durable_before = store.snapshot().unwrap();
    let oversized = "x".repeat(MAX_CATALOG_STRING_BYTES + 1);
    let cases = [
        CloudProfilePatch {
            user_id: "user-1".into(),
            username: "".into(),
            display_name: None,
        },
        CloudProfilePatch {
            user_id: "user-1".into(),
            username: oversized.clone(),
            display_name: None,
        },
        CloudProfilePatch {
            user_id: "user-1".into(),
            username: "valid".into(),
            display_name: Some(" ".into()),
        },
        CloudProfilePatch {
            user_id: "user-1".into(),
            username: "valid".into(),
            display_name: Some(oversized),
        },
    ];

    for patch in cases {
        let error = port
            .apply_profile(
                &current.snapshot.account_key,
                current.snapshot.generation,
                patch,
            )
            .unwrap_err();
        assert!(!error.is_account_changed());
        assert_eq!(store.snapshot().unwrap(), durable_before);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_port_marks_profile_fsync_as_blocking_on_multithread_tokio() {
    let (_directory, store) = connected_store("profile-blocking-boundary");
    let port = SettingsCloudAccountPort::new(store);
    let current = port.snapshot().unwrap();

    let updated = port
        .apply_profile(
            &current.snapshot.account_key,
            current.snapshot.generation,
            CloudProfilePatch {
                user_id: "user-1".into(),
                username: "runtime-safe".into(),
                display_name: None,
            },
        )
        .unwrap();

    assert_eq!(updated.snapshot.username.as_deref(), Some("runtime-safe"));
}

#[test]
fn cache_fence_derivation_matches_user_username_and_credential_fallbacks() {
    let settings = connected_settings();
    let user_fence = cloud_cache_account_fence_from_snapshot(&snapshot(settings.clone())).unwrap();
    assert_eq!(
        user_fence.cache_namespace,
        CloudCacheNamespace::derive("https://cloud.example", "user-1").unwrap()
    );

    let mut username_only = settings.clone();
    username_only.cloud.connected_user_id = None;
    let username_fence =
        cloud_cache_account_fence_from_snapshot(&snapshot(username_only.clone())).unwrap();
    assert_eq!(
        username_fence.cache_namespace,
        CloudCacheNamespace::derive("https://cloud.example", "clipper").unwrap()
    );

    username_only.cloud.connected_username = None;
    let credential = username_only.cloud.credential_target.clone().unwrap();
    let credential_fence =
        cloud_cache_account_fence_from_snapshot(&snapshot(username_only)).unwrap();
    assert_eq!(
        credential_fence.cache_namespace,
        CloudCacheNamespace::derive("https://cloud.example", &credential).unwrap()
    );

    let mut unavailable = settings;
    unavailable.cloud.connected_user_id = None;
    unavailable.cloud.connected_username = None;
    unavailable.cloud.credential_target = None;
    assert!(cloud_cache_account_fence_from_snapshot(&snapshot(unavailable)).is_err());
}

#[test]
fn cache_fence_helper_rejects_unbounded_or_inconsistent_service_accounts() {
    let mut oversized = connected_settings();
    oversized.cloud.connected_username = Some("x".repeat(MAX_CATALOG_STRING_BYTES + 1));
    assert!(cloud_cache_account_fence_from_snapshot(&snapshot(oversized)).is_err());

    let mut service = cloud_service_account_from_snapshot(&snapshot(connected_settings())).unwrap();
    service.snapshot.account_key = CloudAccountKey::new("forged-account").unwrap();
    assert!(cloud_cache_account_fence_from_service_account(&service).is_err());
}

#[test]
fn settings_publication_guard_compares_key_generation_and_namespace_before_invoking() {
    let (_directory, store) = connected_store("cache-publication-fences");
    let guard = SettingsAccountPublicationGuard::new(store.clone());
    let current = cloud_cache_account_fence_from_snapshot(&store.snapshot().unwrap()).unwrap();
    assert!(guard.is_current(&current));

    let mut publications = 0;
    guard
        .publish_if_current(&current, &mut || {
            publications += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(publications, 1);

    let mut stale_key = current.clone();
    stale_key.account_key = CloudAccountKey::new("wrong-account").unwrap();
    let mut stale_generation = current.clone();
    stale_generation.account_generation =
        CloudAccountGeneration::new(stale_generation.account_generation.get() + 1);
    let mut stale_namespace = current.clone();
    stale_namespace.cache_namespace =
        CloudCacheNamespace::derive("https://other.example", "user-1").unwrap();
    for stale in [stale_key, stale_generation, stale_namespace] {
        assert!(!guard.is_current(&stale));
        let error = guard
            .publish_if_current(&stale, &mut || {
                publications += 1;
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error, CloudCacheError::StaleAccount);
    }
    assert_eq!(publications, 1);
}

#[test]
fn settings_publication_guard_tolerates_unrelated_writes_and_propagates_publication_errors() {
    let (directory, store) = connected_store("cache-publication-unrelated");
    let guard = SettingsAccountPublicationGuard::new(store.clone());
    let before = store.snapshot().unwrap();
    let current = cloud_cache_account_fence_from_snapshot(&before).unwrap();
    store
        .transact(SettingsTransaction {
            expected_revision: before.revision,
            expected_account_generation: before.account_generation,
            change: SettingsChange::SetMediaRoot(
                directory.path().join("unrelated").display().to_string(),
            ),
        })
        .unwrap();
    assert!(guard.is_current(&current));

    let expected = CloudCacheError::Download("publication failed".into());
    let error = guard
        .publish_if_current(&current, &mut || Err(expected.clone()))
        .unwrap_err();
    assert_eq!(error, expected);
}

#[test]
fn settings_publication_guard_rejects_switch_away_and_back_aba() {
    let (_directory, store) = connected_store("cache-publication-aba");
    let guard = SettingsAccountPublicationGuard::new(store.clone());
    let account_a = store.snapshot().unwrap();
    let stale = cloud_cache_account_fence_from_snapshot(&account_a).unwrap();

    let mut account_b = account_a.document.cloud.clone();
    account_b.host_url = "https://b.example".into();
    account_b.connected_user_id = Some("user-b".into());
    account_b.connected_username = Some("other".into());
    account_b.credential_target = Some(clipline_settings::cloud::cloud_credential_target(
        &account_b.host_url,
        "user-b",
    ));
    let switched = store
        .transact(SettingsTransaction {
            expected_revision: account_a.revision,
            expected_account_generation: account_a.account_generation,
            change: SettingsChange::ReplaceCloudSettings(account_b),
        })
        .unwrap();
    let restored = store
        .transact(SettingsTransaction {
            expected_revision: switched.revision,
            expected_account_generation: switched.account_generation,
            change: SettingsChange::ReplaceCloudSettings(account_a.document.cloud),
        })
        .unwrap();
    let current = cloud_cache_account_fence_from_snapshot(&restored).unwrap();
    assert_eq!(current.account_key, stale.account_key);
    assert_eq!(current.cache_namespace, stale.cache_namespace);
    assert_ne!(current.account_generation, stale.account_generation);

    let mut invoked = false;
    assert!(!guard.is_current(&stale));
    let error = guard
        .publish_if_current(&stale, &mut || {
            invoked = true;
            Ok(())
        })
        .unwrap_err();
    assert_eq!(error, CloudCacheError::StaleAccount);
    assert!(!invoked);
}

#[test]
fn settings_publication_guard_rejects_legacy_namespace_aba_with_the_same_account_key() {
    let (_directory, store) = connected_store("cache-publication-legacy-namespace-aba");
    let initial = store.snapshot().unwrap();
    let mut legacy_a = initial.document.cloud.clone();
    legacy_a.connected_user_id = None;
    legacy_a.connected_username = Some("legacy-a".into());
    legacy_a.credential_target = Some(clipline_settings::cloud::cloud_credential_target(
        &legacy_a.host_url,
        "stable-credential-owner",
    ));
    let connected_a = store
        .transact(SettingsTransaction {
            expected_revision: initial.revision,
            expected_account_generation: initial.account_generation,
            change: SettingsChange::ReplaceCloudSettings(legacy_a.clone()),
        })
        .unwrap();
    let guard = SettingsAccountPublicationGuard::new(store.clone());
    let stale = cloud_cache_account_fence_from_snapshot(&connected_a).unwrap();

    let mut legacy_b = legacy_a.clone();
    legacy_b.connected_username = Some("legacy-b".into());
    let switched = store
        .transact(SettingsTransaction {
            expected_revision: connected_a.revision,
            expected_account_generation: connected_a.account_generation,
            change: SettingsChange::ReplaceCloudSettings(legacy_b),
        })
        .unwrap();
    let restored = store
        .transact(SettingsTransaction {
            expected_revision: switched.revision,
            expected_account_generation: switched.account_generation,
            change: SettingsChange::ReplaceCloudSettings(legacy_a),
        })
        .unwrap();
    let current = cloud_cache_account_fence_from_snapshot(&restored).unwrap();

    assert_eq!(current.account_key, stale.account_key);
    assert_eq!(current.cache_namespace, stale.cache_namespace);
    assert_ne!(current.account_generation, stale.account_generation);
    let mut invoked = false;
    assert!(!guard.is_current(&stale));
    assert_eq!(
        guard
            .publish_if_current(&stale, &mut || {
                invoked = true;
                Ok(())
            })
            .unwrap_err(),
        CloudCacheError::StaleAccount
    );
    assert!(!invoked);
}
