use std::collections::BTreeMap;

use clipline_library::{
    account_key, merge_cloud_library_entries, plain_http_confirmed, reconcile_upload_progress,
    record_uploaded, share_url, CloudAccountFields, CloudLibraryItem, CloudRequestGate,
    CloudSettingsModel, CloudUploadRecord, NullablePatch, UploadProgressPatch,
};

fn record(id: &str, path: &str, status: &str, updated_at_unix: u64) -> CloudUploadRecord {
    CloudUploadRecord {
        local_clip_id: id.into(),
        path: path.into(),
        remote_clip_id: None,
        remote_url: None,
        visibility: "private".into(),
        upload_status: status.into(),
        error: None,
        updated_at_unix,
    }
}

#[test]
fn request_gate_is_account_scoped_and_checked() {
    let first_key = account_key(&CloudAccountFields {
        host_url: "https://clips.example".into(),
        connected_user_id: "user-a".into(),
        credential_target: "credential-a".into(),
    })
    .unwrap();
    let replacement_key = account_key(&CloudAccountFields {
        connected_user_id: "user-b".into(),
        credential_target: "credential-b".into(),
        ..CloudAccountFields::default()
    })
    .unwrap();
    let mut gate = CloudRequestGate::default();
    let first = gate.begin(first_key.clone()).unwrap();
    let second = gate.begin(first_key.clone()).unwrap();
    assert!(!gate.is_current(&first, &first_key));
    assert!(gate.is_current(&second, &first_key));
    assert!(!gate.is_current(&second, &replacement_key));
    assert_eq!(gate.invalidate().unwrap().get(), 3);
    assert!(!gate.is_current(&second, &first_key));
}

#[test]
fn backend_merge_changes_only_backend_owned_cloud_fields() {
    let mut local = CloudSettingsModel {
        host_url: Some("https://old.example".into()),
        public_url: None,
        connected_user_id: Some("old-user".into()),
        connected_username: Some("old-name".into()),
        connected_display_name: None,
        credential_target: Some("old-credential".into()),
        default_visibility: "unlisted".into(),
        delete_local_after_upload: true,
        auto_upload_rules: false,
        uploads: BTreeMap::from([(
            "old".into(),
            record("old", "C:/Clips/old.mp4", "uploaded_public", 1),
        )]),
    };
    let backend = CloudSettingsModel {
        host_url: Some("https://new.example".into()),
        public_url: Some("https://clips.example".into()),
        connected_user_id: Some("new-user".into()),
        connected_username: Some("new-name".into()),
        connected_display_name: Some("New Name".into()),
        credential_target: Some("new-credential".into()),
        default_visibility: "private".into(),
        delete_local_after_upload: false,
        auto_upload_rules: true,
        uploads: BTreeMap::from([(
            "fresh".into(),
            record("fresh", "C:/Clips/fresh.mp4", "queued", 2),
        )]),
    };

    local.merge_backend_owned(&backend).unwrap();
    assert_eq!(local.host_url.as_deref(), Some("https://new.example"));
    assert_eq!(local.connected_user_id.as_deref(), Some("new-user"));
    assert_eq!(local.default_visibility, "unlisted");
    assert!(local.delete_local_after_upload);
    assert!(!local.auto_upload_rules);
    assert_eq!(local.uploads.keys().collect::<Vec<_>>(), vec!["fresh"]);
}

#[test]
fn consent_upload_and_share_rules_match_cloud_core() {
    assert!(plain_http_confirmed(
        "http://clips.local",
        "http://clips.local",
        true
    ));
    assert!(!plain_http_confirmed(
        "http://clips.local",
        "http://other.local",
        true
    ));
    assert!(!plain_http_confirmed("", "", true));

    let mut private = record("private", "C:/Clips/private.mp4", "uploaded_private", 1);
    private.remote_clip_id = Some("remote-private".into());
    private.remote_url = Some("https://clips.example/c/private".into());
    assert!(record_uploaded(&private));
    assert_eq!(share_url(&private), "");

    private.visibility = "unlisted".into();
    assert_eq!(share_url(&private), "https://clips.example/c/private");
    private.upload_status = "processing".into();
    assert!(!record_uploaded(&private));
    assert_eq!(share_url(&private), "");
}

#[test]
fn byte_only_progress_does_not_rebuild_cards() {
    let current = record("local-1", "C:/Clips/one.mp4", "uploading", 100);
    let bytes = reconcile_upload_progress(
        &current,
        &UploadProgressPatch {
            received_size_bytes: Some(500),
            file_size_bytes: Some(1_000),
            ..UploadProgressPatch::default()
        },
        "unlisted",
        200,
    )
    .unwrap();
    assert!(!bytes.render_required);
    assert_eq!(bytes.record.updated_at_unix, 100);

    let processing = reconcile_upload_progress(
        &bytes.record,
        &UploadProgressPatch {
            upload_status: Some("processing".into()),
            ..UploadProgressPatch::default()
        },
        "unlisted",
        201,
    )
    .unwrap();
    assert!(processing.render_required);
    assert_eq!(processing.record.updated_at_unix, 201);
}

#[test]
fn first_progress_and_explicit_nulls_match_javascript_patch_semantics() {
    let first = reconcile_upload_progress(
        &CloudUploadRecord::default(),
        &UploadProgressPatch {
            local_clip_id: Some("local-1".into()),
            path: Some("C:/Clips/one.mp4".into()),
            remote_clip_id: NullablePatch::Value(Some("remote-1".into())),
            remote_url: NullablePatch::Value(Some("https://clips.example/one".into())),
            upload_status: Some("queued".into()),
            ..UploadProgressPatch::default()
        },
        "unlisted",
        10,
    )
    .unwrap();
    assert!(first.render_required);
    assert_eq!(first.record.visibility, "unlisted");

    let cleared = reconcile_upload_progress(
        &first.record,
        &UploadProgressPatch {
            remote_url: NullablePatch::Value(None),
            error: NullablePatch::Value(None),
            ..UploadProgressPatch::default()
        },
        "private",
        11,
    )
    .unwrap();
    assert!(cleared.render_required);
    assert_eq!(cleared.record.remote_url, None);
    assert_eq!(cleared.record.visibility, "unlisted");
}

#[test]
fn nullable_progress_fields_distinguish_missing_from_explicit_null_on_the_wire() {
    let mut current = record("local-1", "C:/Clips/one.mp4", "uploading", 7);
    current.remote_url = Some("https://clips.example/one".into());
    current.error = Some("old error".into());

    let missing: UploadProgressPatch = serde_json::from_str("{}").unwrap();
    assert_eq!(serde_json::to_string(&missing).unwrap(), "{}");
    let preserved = reconcile_upload_progress(&current, &missing, "private", 8).unwrap();
    assert_eq!(preserved.record.remote_url, current.remote_url);
    assert_eq!(preserved.record.error, current.error);

    let nulls: UploadProgressPatch =
        serde_json::from_str(r#"{"remote_url":null,"error":null}"#).unwrap();
    assert_eq!(
        serde_json::to_value(&nulls).unwrap(),
        serde_json::json!({ "remote_url": null, "error": null })
    );
    let cleared = reconcile_upload_progress(&current, &nulls, "private", 9).unwrap();
    assert_eq!(cleared.record.remote_url, None);
    assert_eq!(cleared.record.error, None);
    assert!(cleared.render_required);

    let value: UploadProgressPatch =
        serde_json::from_str(r#"{"remote_url":"https://clips.example/new"}"#).unwrap();
    assert_eq!(
        serde_json::to_value(&value).unwrap(),
        serde_json::json!({ "remote_url": "https://clips.example/new" })
    );
}

#[test]
fn byte_only_progress_uses_now_when_the_current_timestamp_is_zero() {
    let current = record("local-1", "C:/Clips/one.mp4", "uploading", 0);
    let reconciled = reconcile_upload_progress(
        &current,
        &UploadProgressPatch {
            received_size_bytes: Some(1),
            file_size_bytes: Some(2),
            ..UploadProgressPatch::default()
        },
        "private",
        44,
    )
    .unwrap();
    assert!(!reconciled.render_required);
    assert_eq!(reconciled.record.updated_at_unix, 44);
}

#[test]
fn authoritative_merge_prefers_server_and_keeps_only_active_missing_uploads() {
    let mut known = record(
        "localKnown",
        "C:/Clips/local known.mp4",
        "uploaded_private",
        10,
    );
    known.remote_clip_id = Some("remote-known-old".into());
    known.remote_url = Some("https://clips.example/old-known".into());
    let mut history = record(
        "localOnlyHistory",
        "C:/Clips/local history.mp4",
        "uploaded_public",
        20,
    );
    history.remote_clip_id = Some("remote-history".into());
    history.remote_url = Some("https://clips.example/history".into());
    history.visibility = "public".into();
    let mut active = record("active", "C:/Clips/active.mp4", "uploaded_processing", 15);
    active.remote_clip_id = Some("remote-active".into());

    let cloud = vec![CloudLibraryItem {
        remote_clip_id: "remote-known".into(),
        local_clip_id: Some("localKnown".into()),
        path: String::new(),
        title: "Server Known".into(),
        remote_url: String::new(),
        visibility: "private".into(),
        upload_status: "uploaded_private".into(),
        updated_at_unix: 40,
        uploaded_at_unix: None,
        duration_ms: Some(2_500),
        file_size_bytes: Some(500),
        source_type: None,
    }];
    let entries = merge_cloud_library_entries(
        &[known, history, active],
        &[
            "C:/Clips/local known.mp4".into(),
            "C:/Clips/local history.mp4".into(),
        ],
        &cloud,
        true,
    )
    .unwrap();

    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].title, "Server Known");
    assert_eq!(entries[0].path, "C:/Clips/local known.mp4");
    assert!(entries[0].local_available);
    assert_eq!(entries[0].duration_ms, Some(2_500));
    assert_eq!(entries[1].local_clip_id, "active");
    assert_eq!(entries[1].upload_status, "uploaded_processing");
}

#[test]
fn merge_uses_windows_identity_and_deterministic_tie_breaks() {
    let mut alpha = record(
        "alpha",
        r"\\?\D:\Videos\Clipline\clip.mp4",
        "uploaded_public",
        10,
    );
    alpha.remote_clip_id = Some("remote-alpha".into());
    alpha.visibility = "public".into();
    let mut beta = record("beta", "C:/Clips/clip.mp4", "uploaded_private", 10);
    beta.remote_clip_id = Some("remote-beta".into());

    let entries = merge_cloud_library_entries(
        &[beta, alpha],
        &[r"D:\Videos\Clipline\clip.mp4".into()],
        &[],
        false,
    )
    .unwrap();
    assert_eq!(entries[0].local_clip_id, "alpha");
    assert_eq!(entries[0].title, "clip");
    assert!(entries[0].local_available);
    assert_eq!(entries[1].local_clip_id, "beta");
}

#[test]
fn remote_url_only_server_rows_keep_the_explicit_empty_remote_id() {
    let entries = merge_cloud_library_entries(
        &[],
        &[],
        &[CloudLibraryItem {
            remote_clip_id: String::new(),
            local_clip_id: None,
            path: String::new(),
            title: "URL only".into(),
            remote_url: "https://clips.example/url-only".into(),
            visibility: "unlisted".into(),
            upload_status: "uploaded_public".into(),
            updated_at_unix: 1,
            uploaded_at_unix: None,
            duration_ms: None,
            file_size_bytes: None,
            source_type: None,
        }],
        true,
    )
    .unwrap();
    assert_eq!(entries[0].remote_clip_id.as_deref(), Some(""));
    assert_eq!(
        serde_json::to_value(&entries[0]).unwrap()["remote_clip_id"],
        ""
    );
}
