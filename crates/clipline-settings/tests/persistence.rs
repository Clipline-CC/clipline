use std::sync::{Arc, Barrier};

use clipline_settings::cloud::{cloud_credential_target, CloudUploadRecord};
use clipline_settings::osu::osu_credential_target;
use clipline_settings::{
    AccountGeneration, CloudAccountIdentity, CloudRecordCas, CloudRecordCasKind, CloudRecordSlot,
    SettingsChange, SettingsProfile, SettingsStore, SettingsTransaction, SettingsTransactionError,
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
