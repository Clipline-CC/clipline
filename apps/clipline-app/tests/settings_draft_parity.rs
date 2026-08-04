use std::fs;
use std::path::{Path, PathBuf};

use boa_engine::{Context, Source};
use serde_json::{json, Value};

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/slint/settings-draft-parity.json")
}

fn fixture() -> Value {
    serde_json::from_str(&fs::read_to_string(fixture_path()).expect("read settings parity fixture"))
        .expect("parse settings parity fixture")
}

fn context() -> Context {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/settings.js");
    let source = fs::read_to_string(path).expect("read ui/settings.js");
    let mut context = Context::default();
    context
        .eval(Source::from_bytes(&source))
        .expect("settings.js evaluates without DOM or Tauri globals");
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

fn settings_value(vector: &Value, side: &str) -> Value {
    let value = &vector[side];
    json!({
        "open_on_startup": value["openOnStartup"],
        "close_to_tray": value["closeToTray"],
        "capture_backend": value["captureBackend"],
        "capture_region": { "x": value["captureX"] },
        "audio": { "mic_enabled": value["micEnabled"] },
        "replay_window_s": value["replayWindow"],
        "disk_quota_gb": value["diskQuota"],
        "hotkey": value["hotkey"],
        "games": { "auto_detect": value["gamesAutoDetect"] },
        "cloud": {
            "default_visibility": value["visibility"],
            "uploads": {
                "fixture-clip": {
                    "status": value["uploadStatus"]
                }
            }
        }
    })
}

#[test]
fn retained_javascript_matches_every_frozen_dirty_vector() {
    let fixture = fixture();
    assert_eq!(fixture["schemaVersion"], 1);
    let mut context = context();
    for vector in fixture["dirtyVectors"].as_array().unwrap() {
        let baseline = settings_value(vector, "baseline");
        let draft = settings_value(vector, "draft");
        let expression = format!("SettingsDraftCore.dirty({baseline}, {draft})");
        assert_eq!(
            eval(&mut context, &expression),
            vector["expectedDirty"].to_string(),
            "{}",
            vector["name"]
        );
    }
}

#[test]
fn retained_javascript_matches_every_frozen_close_vector() {
    let fixture = fixture();
    let mut context = context();
    for vector in fixture["closeVectors"].as_array().unwrap() {
        let input = json!({
            "dirty": vector["dirty"],
            "warningArmed": vector["warningArmed"],
            "allowDiscard": vector["allowDiscard"],
        });
        let expression = format!("SettingsDraftCore.closeAction({input})");
        assert_eq!(
            eval(&mut context, &expression),
            vector["expected"].as_str().unwrap(),
            "{}",
            vector["name"]
        );
    }
}

#[test]
fn retained_webview_tab_order_matches_the_frozen_rust_contract() {
    let fixture = fixture();
    let html = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/index.html"))
        .expect("read ui/index.html");
    let mut previous = 0;
    for (index, tab) in fixture["tabOrder"].as_array().unwrap().iter().enumerate() {
        let needle = format!("data-tab=\"{}\"", tab.as_str().unwrap());
        let position = html
            .find(&needle)
            .unwrap_or_else(|| panic!("missing {needle}"));
        if index > 0 {
            assert!(position > previous, "{needle} is out of frozen order");
        }
        previous = position;
    }
}
