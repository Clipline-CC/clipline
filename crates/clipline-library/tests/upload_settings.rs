use std::sync::Arc;

use clipline_library::{
    local_clip_id_for_source, upload_account_owner_from_snapshot, upload_record_to_cloud_record,
    ActiveFileRegistry, CloudAccountGeneration, DurableUploadToken, LocalLibraryRepository,
    SettingsUploadAccountFence, SettingsUploadRecordPort, StandardRepositoryFileSystem,
    UploadAccountFence, UploadGeneration, UploadPhase, UploadRecord, UploadRecordErrorKind,
    UploadRecordPort,
};
use clipline_settings::{
    AppSettings, CloudAccountIdentity, SettingsChange, SettingsProfile, SettingsStore,
    SettingsTransaction,
};
use clipline_test_utils::TestDir;

fn connected_store(name: &str) -> (TestDir, SettingsStore) {
    let directory = TestDir::new("clipline-upload-settings", name);
    let profile = SettingsProfile::isolated(directory.path());
    let mut settings = AppSettings {
        media_dir: profile.default_media_dir().display().to_string(),
        ..AppSettings::default()
    };
    settings.cloud.host_url = "https://cloud.example".into();
    settings.cloud.connected_user_id = Some("user-1".into());
    settings.cloud.credential_target = Some("credential-1".into());
    settings.save_to(profile.settings_path()).unwrap();
    (directory, SettingsStore::open(profile))
}

fn validated_source(directory: &TestDir) -> clipline_library::ValidatedClipPath {
    let root = directory.path().join("media");
    std::fs::create_dir_all(&root).unwrap();
    let clip = root.join("clip.mp4");
    std::fs::write(&clip, b"clip").unwrap();
    LocalLibraryRepository::with_seams(
        &root,
        Arc::new(StandardRepositoryFileSystem),
        Arc::new(ActiveFileRegistry::new()),
    )
    .unwrap()
    .validate_clip_path(&clip.display().to_string())
    .unwrap()
}

fn record(
    owner: &clipline_library::UploadAccountOwner,
    source: &clipline_library::ValidatedClipPath,
    generation: UploadGeneration,
) -> UploadRecord {
    let local_clip_id = local_clip_id_for_source(source.file_identity());
    UploadRecord {
        token: DurableUploadToken {
            account_key: owner.account_key.clone(),
            account_generation: owner.account_generation,
            upload_generation: generation,
            local_clip_id,
            source_path: source.comparison_identity().clone(),
        },
        client_clip_id: None,
        path: source.display_path().to_owned(),
        visibility: "private".into(),
        phase: UploadPhase::Queued,
        upload_status: "queued".into(),
        received_size_bytes: 17,
        file_size_bytes: 101,
        remote_clip_id: None,
        remote_url: None,
        error: None,
        local_deleted: false,
        updated_at_unix: 7,
    }
}

#[test]
fn settings_account_fence_rejects_a_replacement_login() {
    let (_directory, store) = connected_store("account-fence");
    let snapshot = store.snapshot().unwrap();
    let owner = upload_account_owner_from_snapshot(&snapshot).unwrap();
    let fence = SettingsUploadAccountFence::new(store.clone());
    assert!(fence.is_current(&owner));

    let mut cloud = snapshot.document.cloud;
    cloud.connected_user_id = Some("user-2".into());
    let changed = store
        .transact(SettingsTransaction {
            expected_revision: snapshot.revision,
            expected_account_generation: snapshot.account_generation,
            change: SettingsChange::ReplaceCloudSettings(cloud),
        })
        .unwrap();
    assert_ne!(
        changed.account,
        CloudAccountIdentity::from_settings(&AppSettings::default().cloud)
    );
    assert!(!fence.is_current(&owner));
    assert_eq!(
        upload_account_owner_from_snapshot(&changed)
            .unwrap()
            .account_generation,
        CloudAccountGeneration::new(changed.account_generation.get())
    );
}

#[test]
fn settings_record_port_uses_exact_cas_and_advances_durable_generations() {
    let (directory, store) = connected_store("exact-cas");
    let source = validated_source(&directory);
    let owner = upload_account_owner_from_snapshot(&store.snapshot().unwrap()).unwrap();
    let port = SettingsUploadRecordPort::new(store.clone());
    let local_clip_id = local_clip_id_for_source(source.file_identity());

    let generation = port
        .allocate_generation(&owner, &local_clip_id, &source)
        .unwrap();
    assert_eq!(generation, UploadGeneration::new(1));
    let queued = record(&owner, &source, generation);
    let cursor = port.admit(queued.clone()).unwrap();
    assert_eq!(
        store
            .snapshot()
            .unwrap()
            .document
            .cloud
            .uploads
            .get(local_clip_id.as_str()),
        Some(&upload_record_to_cloud_record(&queued))
    );

    // Volatile byte counters survive exact in-process reloads even though the
    // compatibility settings schema intentionally does not persist them.
    assert_eq!(
        port.load(&owner, &local_clip_id)
            .unwrap()
            .unwrap()
            .record
            .received_size_bytes,
        17
    );

    let mut uploading = queued;
    uploading.phase = UploadPhase::Uploading;
    uploading.upload_status = "uploading".into();
    uploading.received_size_bytes = 53;
    let advanced = port.compare_exchange(&cursor, uploading.clone()).unwrap();
    let stale = port.compare_exchange(&cursor, uploading).unwrap_err();
    assert_eq!(stale.kind(), UploadRecordErrorKind::Superseded);

    assert_eq!(
        port.allocate_generation(&owner, &local_clip_id, &source)
            .unwrap(),
        UploadGeneration::new(2)
    );
    port.remove_exact(&advanced).unwrap();
    assert!(port.load(&owner, &local_clip_id).unwrap().is_none());
    assert_eq!(
        port.allocate_generation(&owner, &local_clip_id, &source)
            .unwrap(),
        UploadGeneration::new(2),
        "exact removal must not make a durable upload generation reusable"
    );
}

#[test]
fn every_record_operation_rejects_a_replacement_login() {
    let (directory, store) = connected_store("record-port-account-fence");
    let source = validated_source(&directory);
    let owner = upload_account_owner_from_snapshot(&store.snapshot().unwrap()).unwrap();
    let port = SettingsUploadRecordPort::new(store.clone());
    let local_clip_id = local_clip_id_for_source(source.file_identity());
    let queued = record(
        &owner,
        &source,
        port.allocate_generation(&owner, &local_clip_id, &source)
            .unwrap(),
    );
    let cursor = port.admit(queued.clone()).unwrap();

    let before = store.snapshot().unwrap();
    let mut replacement_account = before.document.cloud;
    replacement_account.connected_user_id = Some("user-2".into());
    store
        .transact(SettingsTransaction {
            expected_revision: before.revision,
            expected_account_generation: before.account_generation,
            change: SettingsChange::ReplaceCloudSettings(replacement_account),
        })
        .unwrap();

    assert_eq!(
        port.allocate_generation(&owner, &local_clip_id, &source)
            .unwrap_err()
            .kind(),
        UploadRecordErrorKind::AccountChanged
    );
    assert_eq!(
        port.load(&owner, &local_clip_id).unwrap_err().kind(),
        UploadRecordErrorKind::AccountChanged
    );
    assert_eq!(
        port.admit(queued).unwrap_err().kind(),
        UploadRecordErrorKind::AccountChanged
    );
    let mut replacement = cursor.record.clone();
    replacement.phase = UploadPhase::Uploading;
    replacement.upload_status = "uploading".into();
    assert_eq!(
        port.compare_exchange(&cursor, replacement)
            .unwrap_err()
            .kind(),
        UploadRecordErrorKind::AccountChanged
    );
    assert_eq!(
        port.remove_exact(&cursor).unwrap_err().kind(),
        UploadRecordErrorKind::AccountChanged
    );
}

#[test]
fn same_path_replacement_gets_a_new_identity_and_later_durable_generation() {
    let (directory, store) = connected_store("same-path-replacement-generation");
    let first_source = validated_source(&directory);
    let owner = upload_account_owner_from_snapshot(&store.snapshot().unwrap()).unwrap();
    let port = SettingsUploadRecordPort::new(store.clone());
    let first_local_id = local_clip_id_for_source(first_source.file_identity());
    let generation = port
        .allocate_generation(&owner, &first_local_id, &first_source)
        .unwrap();
    let cursor = port
        .admit(record(&owner, &first_source, generation))
        .unwrap();
    port.remove_exact(&cursor).unwrap();

    let clip_path = first_source.canonical_path().to_path_buf();
    drop(first_source);
    std::fs::remove_file(&clip_path).unwrap();
    // Consume any immediately reusable filesystem identity before recreating
    // the same display path.
    for index in 0..4 {
        std::fs::write(
            clip_path.with_file_name(format!("identity-churn-{index}.tmp")),
            b"churn",
        )
        .unwrap();
    }
    std::fs::write(&clip_path, b"replacement clip").unwrap();
    let root = directory.path().join("media");
    let replacement_source = LocalLibraryRepository::with_seams(
        &root,
        Arc::new(StandardRepositoryFileSystem),
        Arc::new(ActiveFileRegistry::new()),
    )
    .unwrap()
    .validate_clip_path(&clip_path.display().to_string())
    .unwrap();
    let replacement_local_id = local_clip_id_for_source(replacement_source.file_identity());

    assert_ne!(replacement_local_id, first_local_id);
    assert_eq!(
        port.allocate_generation(&owner, &replacement_local_id, &replacement_source)
            .unwrap(),
        UploadGeneration::new(2)
    );
}
