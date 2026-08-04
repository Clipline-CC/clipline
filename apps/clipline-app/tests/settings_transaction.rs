use std::fs;
use std::path::Path;

fn app_source() -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/app.rs"))
        .expect("read Tauri app source")
}

fn save_settings_body(source: &str) -> &str {
    let start = source
        .find("fn save_settings<R: Runtime>(")
        .expect("shipping save_settings command");
    let end = source[start..]
        .find("\npub fn run(")
        .map(|offset| start + offset)
        .expect("run entry point after save_settings");
    &source[start..end]
}

#[test]
fn shipping_save_command_is_an_adapter_over_the_shared_coordinator() {
    let source = app_source();
    let save = save_settings_body(&source);

    assert!(save.contains("SettingsApplyCoordinator"));
    assert!(save.contains("SettingsPreferences::from_document"));
    assert!(save.contains("coordinator\n        .apply"));
    assert!(save.contains("Ok(success.snapshot.document)"));
}

#[test]
fn shipping_save_command_does_not_restore_a_stale_whole_document() {
    let source = app_source();
    let save = save_settings_body(&source);

    for forbidden in [
        "CLOUD_SETTINGS_SAVE_LOCK",
        "preserve_backend_owned_settings_fields",
        "ReplaceDocument",
        "rollback_persisted_settings",
        "persist_ui_preferences",
    ] {
        assert!(
            !save.contains(forbidden),
            "save_settings must not contain the legacy transaction path: {forbidden}"
        );
    }
}

#[test]
fn post_persistence_runtime_commit_is_not_fallible() {
    let source = app_source();
    let start = source
        .find("fn finish_prepared_restart<R: Runtime>(")
        .expect("runtime commit helper");
    let end = source[start..]
        .find("\n    fn request_save")
        .map(|offset| start + offset)
        .expect("request_save after runtime commit");
    let commit = &source[start..end];

    assert!(commit.contains("commit_with_options"));
    assert!(!commit.contains("-> Result"));
    assert!(!commit.contains("service::spawn"));
    let join = commit
        .find("old_pump.join()")
        .expect("join prior event pump");
    let start = commit
        .find("commit_with_options")
        .expect("release prepared recorder latch");
    assert!(
        join < start,
        "old recorder must join before replacement starts"
    );
}
