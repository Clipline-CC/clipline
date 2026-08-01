use std::fs;
use std::path::{Path, PathBuf};

const REQUIRED_SURFACES: &[&str] = &[
    "surface:persistent-shell",
    "surface:local-library",
    "surface:cloud-library",
    "surface:review",
    "surface:settings-general",
    "surface:settings-capture",
    "surface:settings-recording",
    "surface:settings-games",
    "surface:settings-storage",
    "surface:settings-cloud",
    "surface:settings-hotkeys",
    "surface:settings-support",
];

const REQUIRED_DIALOGS: &[&str] = &[
    "dialog:delete-confirmation",
    "dialog:quit-confirmation",
    "dialog:update-available",
    "dialog:elevated-game-warning",
    "dialog:cloud-upload",
    "dialog:detected-games",
    "dialog:running-window",
    "dialog:file-rename",
    "dialog:game-plugin-settings",
    "dialog:shortcut-guide",
    "dialog:media-folder-picker",
    "dialog:replay-cache-folder-picker",
    "dialog:support-bundle-picker",
];

const REQUIRED_SHORTCUTS: &[&str] = &[
    "shortcut:global-save-replay",
    "shortcut:play-pause",
    "shortcut:seek-five-seconds",
    "shortcut:seek-one-second",
    "shortcut:seek-source-frames",
    "shortcut:seek-tenth-second",
    "shortcut:set-trim-in-out",
    "shortcut:marker-navigation",
    "shortcut:edit-point-navigation",
    "shortcut:timeline-zoom",
    "shortcut:timeline-fit",
    "shortcut:clip-boundary",
    "shortcut:toggle-snapping",
    "shortcut:fullscreen",
    "shortcut:escape-context",
    "shortcut:shortcut-guide",
    "shortcut:library-select-all",
    "shortcut:settings-tab-navigation",
];

const REQUIRED_GESTURES: &[&str] = &[
    "gesture:timeline-seek",
    "gesture:trim-edge-drag",
    "gesture:trim-selection-drag",
    "gesture:snap-bypass",
    "gesture:timeline-wheel-zoom",
    "gesture:timeline-pan",
    "gesture:overview-pan",
    "gesture:overview-edge-zoom",
    "gesture:marker-seek",
    "gesture:play-block-seek",
    "gesture:shift-fit-whole-clip",
    "gesture:shift-copy-original",
    "gesture:context-menu",
    "gesture:stage-overlay-activity",
];

const REQUIRED_TRAY_AND_LIFECYCLE: &[&str] = &[
    "tray:open",
    "tray:save-replay",
    "tray:open-diagnostics",
    "tray:quit",
    "tray:left-click-open",
    "lifecycle:normal-launch",
    "lifecycle:autostart-tray",
    "lifecycle:single-instance-reveal",
    "lifecycle:close-to-tray-or-quit",
    "lifecycle:minimize-to-tray-or-taskbar",
    "lifecycle:foreground-bootstrap-snapshot",
];

const REQUIRED_UPDATER_AND_PACKAGE: &[&str] = &[
    "updater:silent-check",
    "updater:manual-check",
    "updater:install",
    "updater:signature-verification",
    "package:regular-nsis",
    "package:standalone-nsis",
    "package:webview2-runtime",
    "package:ffmpeg-resource",
    "package:product-identity",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("app crate should be nested under workspace/apps")
        .to_path_buf()
}

fn read(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path.as_ref())
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.as_ref().display()))
}

fn registered_commands(app_rs: &str) -> Vec<String> {
    let marker = "tauri::generate_handler![";
    let tail = app_rs
        .split_once(marker)
        .expect("production app must register a Tauri command handler")
        .1;
    let body = tail
        .split_once(']')
        .expect("Tauri command handler list must terminate")
        .0;

    body.lines()
        .map(str::trim)
        .map(|line| line.trim_end_matches(','))
        .filter(|line| !line.is_empty() && !line.starts_with("//"))
        .map(|symbol| symbol.rsplit("::").next().unwrap().to_owned())
        .collect()
}

fn first_string_arguments(source: &str, call: &str) -> Vec<String> {
    let mut output = Vec::new();
    let mut remaining = source;

    while let Some(index) = remaining.find(call) {
        remaining = &remaining[index + call.len()..];
        let trimmed = remaining.trim_start();
        let Some(value) = trimmed.strip_prefix('"') else {
            continue;
        };
        let Some(end) = value.find('"') else {
            continue;
        };
        output.push(value[..end].to_owned());
        remaining = &value[end + 1..];
    }

    output.sort();
    output.dedup();
    output
}

fn assert_tokens(ledger: &str, tokens: impl IntoIterator<Item = String>) {
    let missing = tokens
        .into_iter()
        .filter(|token| !ledger.contains(&format!("`{token}`")))
        .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "Slint parity ledger is missing tokens: {}",
        missing.join(", ")
    );
}

#[test]
fn parity_ledger_covers_the_shipping_frontend_boundary() {
    let root = workspace_root();
    let ledger = read(root.join("docs/slint/parity-ledger.md"));
    let app_rs = read(root.join("apps/clipline-app/src/app.rs"));

    let commands = registered_commands(&app_rs);
    assert_eq!(
        commands.len(),
        60,
        "review new/removed production commands and update the ledger baseline"
    );
    assert_tokens(
        &ledger,
        commands
            .into_iter()
            .map(|command| format!("command:{command}")),
    );

    let mut events = first_string_arguments(&app_rs, ".emit(");
    for path in [
        "apps/clipline-app/src/cloud.rs",
        "apps/clipline-app/src/library.rs",
        "apps/clipline-app/src/osu_api.rs",
        "apps/clipline-app/src/app/support.rs",
    ] {
        events.extend(first_string_arguments(&read(root.join(path)), ".emit("));
    }
    events.extend(first_string_arguments(
        &read(root.join("apps/clipline-app/ui/main.js")),
        "listen(",
    ));
    events.sort();
    events.dedup();
    assert_tokens(
        &ledger,
        events.into_iter().map(|event| format!("event:{event}")),
    );

    let required = REQUIRED_SURFACES
        .iter()
        .chain(REQUIRED_DIALOGS)
        .chain(REQUIRED_SHORTCUTS)
        .chain(REQUIRED_GESTURES)
        .chain(REQUIRED_TRAY_AND_LIFECYCLE)
        .chain(REQUIRED_UPDATER_AND_PACKAGE)
        .map(|token| (*token).to_owned());
    assert_tokens(&ledger, required);

    assert!(
        ledger.contains("Baseline commit: `5eea6c3`"),
        "ledger must identify the audited develop baseline"
    );
    assert!(
        ledger.contains(
            "Status values: `not_started`, `in_progress`, `implemented`, `verified`, `waived`"
        ),
        "ledger must define explicit migration states and waiver handling"
    );
}
