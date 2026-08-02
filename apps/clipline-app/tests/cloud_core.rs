use boa_engine::{Context, Source};
use clipline_library::{account_key, CloudAccountFields};
use std::fs;
use std::path::Path;

fn context() -> Context {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/cloud-core.js");
    let source = fs::read_to_string(path).expect("read ui/cloud-core.js");
    let mut context = Context::default();
    context
        .eval(Source::from_bytes(&source))
        .expect("cloud-core.js evaluates without DOM or Tauri globals");
    context
}

fn eval(context: &mut Context, expression: &str) -> String {
    context
        .eval(Source::from_bytes(expression))
        .unwrap_or_else(|error| panic!("eval `{expression}`: {error}"))
        .to_string(context)
        .expect("stringify result")
        .to_std_string_escaped()
}

#[test]
fn request_gate_rejects_superseded_and_invalidated_requests() {
    let mut context = context();
    assert_eq!(
        eval(
            &mut context,
            "const gate = CloudCore.createRequestGate();\
             const first = gate.begin('host|user-a|credential-a');\
             const second = gate.begin('host|user-a|credential-a');\
             JSON.stringify([\
               gate.isCurrent(first, 'host|user-a|credential-a'),\
               gate.isCurrent(second, 'host|user-a|credential-a'),\
               gate.isCurrent(second, 'host|user-b|credential-b'),\
               gate.invalidate(),\
               gate.isCurrent(second, 'host|user-a|credential-a')\
             ])",
        ),
        "[false,true,false,3,false]"
    );
}

#[test]
fn account_change_invalidation_preserves_replacement_request_ownership() {
    let mut context = context();
    assert_eq!(
        eval(
            &mut context,
            "const gate = CloudCore.createRequestGate();\
             const first = gate.begin('host|user-a|credential-a');\
             gate.invalidate();\
             const replacement = gate.begin('host|user-b|credential-b');\
             JSON.stringify([\
               gate.isCurrent(first, 'host|user-a|credential-a'),\
               gate.isCurrent(first, 'host|user-b|credential-b'),\
               gate.isCurrent(replacement, 'host|user-b|credential-b')\
             ])",
        ),
        "[false,false,true]"
    );
}

#[test]
fn account_key_is_stable_and_account_scoped() {
    let mut context = context();
    assert_eq!(
        eval(
            &mut context,
            "CloudCore.accountKey({\
               host_url: 'https://clips.example',\
               connected_user_id: 'user-7',\
               credential_target: 'credential-7'\
             })",
        ),
        "https://clips.example|user-7|credential-7"
    );
    assert_eq!(
        account_key(&CloudAccountFields {
            host_url: "https://clips.example".into(),
            connected_user_id: "user-7".into(),
            credential_target: "credential-7".into(),
        })
        .unwrap()
        .as_str(),
        "https://clips.example|user-7|credential-7"
    );
}

#[test]
fn backend_cloud_merge_preserves_draft_preferences_and_unrelated_settings() {
    let mut context = context();
    assert_eq!(
        eval(
            &mut context,
            "const draft = { replay_window_s: 91, audio: { mic_enabled: true }, cloud: {\
               host_url: 'https://old.example', connected_user_id: 'old-user',\
               connected_username: 'old-name', connected_display_name: null,\
               credential_target: 'old-credential', public_url: null,\
               default_visibility: 'unlisted', delete_local_after_upload: true,\
               auto_upload_rules: false, uploads: { old: { path: 'old.mp4' } }\
             } };\
             const backend = { cloud: {\
               host_url: 'https://new.example', connected_user_id: 'new-user',\
               connected_username: 'new-name', connected_display_name: 'New Name',\
               credential_target: 'new-credential', public_url: 'https://clips.example',\
               default_visibility: 'private', delete_local_after_upload: false,\
               auto_upload_rules: true, uploads: { fresh: { path: 'fresh.mp4' } }\
             } };\
             const merged = CloudCore.mergeBackendCloudSettings(draft, backend);\
             JSON.stringify({\
               replay_window_s: merged.replay_window_s, audio: merged.audio,\
               cloud: merged.cloud, uploadsCloned: merged.cloud.uploads !== backend.cloud.uploads\
             })",
        ),
        r#"{"replay_window_s":91,"audio":{"mic_enabled":true},"cloud":{"host_url":"https://new.example","connected_user_id":"new-user","connected_username":"new-name","connected_display_name":"New Name","credential_target":"new-credential","public_url":"https://clips.example","default_visibility":"unlisted","delete_local_after_upload":true,"auto_upload_rules":false,"uploads":{"fresh":{"path":"fresh.mp4"}}},"uploadsCloned":true}"#
    );
}

#[test]
fn plain_http_confirmation_requires_checked_exact_origin() {
    let mut context = context();
    assert_eq!(
        eval(
            &mut context,
            "JSON.stringify([\
               CloudCore.plainHttpConfirmed('http://clips.local', 'http://clips.local', true),\
               CloudCore.plainHttpConfirmed('http://clips.local', 'http://clips.local', false),\
               CloudCore.plainHttpConfirmed('http://clips.local', 'http://other.local', true),\
               CloudCore.plainHttpConfirmed('http://clips.local:8080', 'http://clips.local', true),\
               CloudCore.plainHttpConfirmed('', '', true)\
             ])",
        ),
        "[true,false,false,false,false]"
    );
}

#[test]
fn remote_identity_and_public_shareability_are_independent() {
    let mut context = context();
    assert_eq!(
        eval(
            &mut context,
            "const privateClip = {\
               remote_clip_id: 'remote-private', remote_url: null,\
               visibility: 'private', upload_status: 'uploaded_private'\
             };\
             const publicClip = {\
               remote_clip_id: 'remote-public', remote_url: 'https://clips.example/c/c_public',\
               visibility: 'public', upload_status: 'uploaded_public'\
             };\
             const unlistedClip = {\
               remote_clip_id: 'remote-unlisted', remote_url: 'https://clips.example/c/c_hidden',\
               visibility: 'unlisted', upload_status: 'uploaded_public'\
             };\
             const processingClip = {\
               remote_clip_id: 'remote-processing', remote_url: null,\
               visibility: 'public', upload_status: 'uploaded_processing'\
             };\
             JSON.stringify({\
               privateUploaded: CloudCore.recordUploaded(privateClip),\
               privateShare: CloudCore.shareUrl(privateClip),\
               publicShare: CloudCore.shareUrl(publicClip),\
               unlistedShare: CloudCore.shareUrl(unlistedClip),\
               processingUploaded: CloudCore.recordUploaded(processingClip),\
               processingShare: CloudCore.shareUrl(processingClip)\
             })",
        ),
        r#"{"privateUploaded":true,"privateShare":"","publicShare":"https://clips.example/c/c_public","unlistedShare":"https://clips.example/c/c_hidden","processingUploaded":true,"processingShare":""}"#
    );
}

#[test]
fn upload_progress_reconciliation_distinguishes_bytes_from_card_state() {
    let mut context = context();
    assert_eq!(
        eval(
            &mut context,
            "const current = {\
               local_clip_id: 'local-1', path: 'C:/Clips/one.mp4',\
               remote_clip_id: 'remote-1', remote_url: 'https://clips/remote-1',\
               visibility: 'private', upload_status: 'uploading', error: null,\
               updated_at_unix: 100\
             };\
             const bytes = CloudCore.reconcileUploadProgress(current, {\
               local_clip_id: 'local-1', path: 'C:/Clips/one.mp4',\
               remote_clip_id: 'remote-1', remote_url: 'https://clips/remote-1',\
               upload_status: 'uploading', received_size_bytes: 500, file_size_bytes: 1000\
             }, 'unlisted', 200);\
             const processing = CloudCore.reconcileUploadProgress(bytes.record, {\
               local_clip_id: 'local-1', upload_status: 'processing'\
             }, 'unlisted', 201);\
             JSON.stringify({ bytes, processing })",
        ),
        r#"{"bytes":{"record":{"local_clip_id":"local-1","path":"C:/Clips/one.mp4","remote_clip_id":"remote-1","remote_url":"https://clips/remote-1","visibility":"private","upload_status":"uploading","error":null,"updated_at_unix":100},"renderRequired":false},"processing":{"record":{"local_clip_id":"local-1","path":"C:/Clips/one.mp4","remote_clip_id":"remote-1","remote_url":"https://clips/remote-1","visibility":"private","upload_status":"processing","error":null,"updated_at_unix":201},"renderRequired":true}}"#
    );
}

#[test]
fn byte_progress_burst_never_requests_gallery_rebuild() {
    let mut context = context();
    assert_eq!(
        eval(
            &mut context,
            "let record = {\
               local_clip_id: 'local-1', path: 'C:/Clips/one.mp4',\
               remote_clip_id: 'remote-1', remote_url: 'https://clips/remote-1',\
               visibility: 'private', upload_status: 'uploading', error: null,\
               updated_at_unix: 100\
             };\
             let renders = 0;\
             for (let i = 1; i <= 500; i += 1) {\
               const update = CloudCore.reconcileUploadProgress(record, {\
                 local_clip_id: 'local-1', path: 'C:/Clips/one.mp4',\
                 remote_clip_id: 'remote-1', remote_url: 'https://clips/remote-1',\
                 upload_status: 'uploading', received_size_bytes: i, file_size_bytes: 500\
               }, 'private', 100 + i);\
               record = update.record;\
               if (update.renderRequired) renders += 1;\
             }\
             JSON.stringify({ renders, updated: record.updated_at_unix })",
        ),
        r#"{"renders":0,"updated":100}"#
    );
}

#[test]
fn upload_identity_error_and_terminal_transitions_render_immediately() {
    let mut context = context();
    assert_eq!(
        eval(
            &mut context,
            "const first = CloudCore.reconcileUploadProgress({}, {\
               local_clip_id: 'local-1', path: 'C:/Clips/one.mp4', upload_status: 'queued'\
             }, 'private', 10);\
             const remote = CloudCore.reconcileUploadProgress(first.record, {\
               local_clip_id: 'local-1', remote_clip_id: 'remote-1',\
               remote_url: 'https://clips/remote-1', upload_status: 'uploading'\
             }, 'private', 11);\
             const failed = CloudCore.reconcileUploadProgress(remote.record, {\
               local_clip_id: 'local-1', upload_status: 'failed', error: 'network lost'\
             }, 'private', 12);\
             const uploaded = CloudCore.reconcileUploadProgress(failed.record, {\
               local_clip_id: 'local-1', upload_status: 'uploaded_private', error: null\
             }, 'private', 13);\
             JSON.stringify([\
               first.renderRequired, remote.renderRequired, failed.renderRequired,\
               uploaded.renderRequired, uploaded.record.updated_at_unix, uploaded.record.error\
             ])",
        ),
        "[true,true,true,true,13,null]"
    );
}
