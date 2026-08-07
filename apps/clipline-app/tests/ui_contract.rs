//! Structural contract for the review player DOM: Clipline owns the controls,
//! the browser owns nothing, and the UI stays split into testable assets.

use std::fs;
use std::io::BufReader;
use std::path::Path;

fn index_html() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/index.html");
    fs::read_to_string(path).expect("read ui/index.html")
}

const APP_UI_JS: &[&str] = &[
    "presentation-core.js",
    "cloud-core.js",
    "gallery-window-core.js",
    "window-lifecycle-core.js",
    "support-core.js",
    "app-core.js",
    "settings.js",
    "library.js",
    "cloud.js",
    "review-player.js",
    "support.js",
    "first-run.js",
    "main.js",
];

fn read_ui_js(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui").join(name);
    fs::read_to_string(path).unwrap_or_else(|err| panic!("read ui/{name}: {err}"))
}

#[test]
fn legacy_buffer_setting_mirrors_the_replay_window() {
    let html = index_html();
    let settings = read_ui_js("settings.js");

    assert!(
        html.contains("<input id=\"set-buffer\" type=\"hidden\" value=\"60\" />"),
        "the hidden compatibility value must match the default replay window"
    );
    assert!(
        settings
            .matches("$(\"set-buffer\").value = replay;")
            .count()
            >= 2
            && settings.contains("buffer_seconds: replay,"),
        "loading, editing, and saving settings must mirror replay_window_s into buffer_seconds"
    );
    assert!(
        !settings.contains("replay + 15")
            && !settings.contains("BUFFER_HEADROOM_S")
            && !settings.contains("replay_window_s) || 60) + 15"),
        "the removed fixed retention headroom must not survive in the UI"
    );
}

/// Concatenated app UI scripts (everything except player-core.js).
fn main_js() -> String {
    APP_UI_JS
        .iter()
        .map(|name| read_ui_js(name))
        .collect::<Vec<_>>()
        .join("\n")
}

fn player_core_js() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/player-core.js");
    fs::read_to_string(path).expect("read ui/player-core.js")
}

fn styles_css() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/styles.css");
    fs::read_to_string(path).expect("read ui/styles.css")
}

fn css_rule_body<'a>(source: &'a str, selector: &str) -> &'a str {
    let selector_start = source
        .find(selector)
        .unwrap_or_else(|| panic!("missing CSS selector {selector}"));
    let body_start = source[selector_start..]
        .find('{')
        .map(|offset| selector_start + offset + 1)
        .unwrap_or_else(|| panic!("missing CSS block for {selector}"));
    let mut depth = 1usize;
    for (offset, ch) in source[body_start..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &source[body_start..body_start + offset];
                }
            }
            _ => {}
        }
    }
    panic!("unterminated CSS block for {selector}");
}

fn css_decl_value<'a>(rule_body: &'a str, property: &str) -> Option<&'a str> {
    rule_body.split(';').find_map(|declaration| {
        let (name, value) = declaration.trim().split_once(':')?;
        (name.trim() == property).then(|| value.trim())
    })
}

fn marker_png_alpha_bounds(asset_dir: &str, name: &str) -> ((u32, u32), (u32, u32)) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(asset_dir)
        .join(name);
    let file = fs::File::open(&path).unwrap_or_else(|err| panic!("open {path:?}: {err}"));
    let decoder = png::Decoder::new(BufReader::new(file));
    let mut reader = decoder
        .read_info()
        .unwrap_or_else(|err| panic!("decode {path:?}: {err}"));
    let mut bytes = vec![0; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut bytes)
        .unwrap_or_else(|err| panic!("read {path:?}: {err}"));
    assert_eq!(
        (info.color_type, info.bit_depth),
        (png::ColorType::Rgba, png::BitDepth::Eight),
        "{name} must stay an 8-bit RGBA PNG so CSS masks use its alpha channel"
    );

    let row_stride = info.width as usize * 4;
    let frame = &bytes[..info.buffer_size()];
    let mut min_x = info.width;
    let mut min_y = info.height;
    let mut max_x = 0;
    let mut max_y = 0;
    let mut found = false;
    for y in 0..info.height {
        for x in 0..info.width {
            let alpha = frame[y as usize * row_stride + x as usize * 4 + 3];
            if alpha > 0 {
                found = true;
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
    }
    assert!(found, "{name} must include non-transparent marker art");
    (
        (info.width, info.height),
        (max_x - min_x + 1, max_y - min_y + 1),
    )
}

fn png_dimensions(asset_dir: &str, name: &str) -> (u32, u32) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(asset_dir)
        .join(name);
    let file = fs::File::open(&path).unwrap_or_else(|err| panic!("open {path:?}: {err}"));
    let decoder = png::Decoder::new(BufReader::new(file));
    let reader = decoder
        .read_info()
        .unwrap_or_else(|err| panic!("decode {path:?}: {err}"));
    (reader.info().width, reader.info().height)
}

fn js_function_body<'a>(source: &'a str, name: &str) -> &'a str {
    let signature = format!("function {name}(");
    let function_start = source
        .find(&signature)
        .unwrap_or_else(|| panic!("missing JavaScript function {name}"));
    let parameters_start = function_start + signature.len();
    let mut parameter_depth = 1usize;
    let parameters_end = source[parameters_start..]
        .char_indices()
        .find_map(|(offset, ch)| match ch {
            '(' => {
                parameter_depth += 1;
                None
            }
            ')' => {
                parameter_depth -= 1;
                (parameter_depth == 0).then_some(parameters_start + offset + 1)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("unterminated JavaScript parameters for {name}"));
    let body_start = source[parameters_end..]
        .find('{')
        .map(|offset| parameters_end + offset + 1)
        .unwrap_or_else(|| panic!("missing JavaScript function body for {name}"));
    let mut depth = 1usize;
    for (offset, ch) in source[body_start..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &source[body_start..body_start + offset];
                }
            }
            _ => {}
        }
    }
    panic!("unterminated JavaScript function body for {name}");
}

fn app_rs() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/app.rs");
    fs::read_to_string(path).expect("read src/app.rs")
}

fn tauri_config() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tauri.conf.json");
    fs::read_to_string(path).expect("read tauri.conf.json")
}

fn tauri_standalone_config() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tauri.standalone.conf.json");
    fs::read_to_string(path).expect("read tauri.standalone.conf.json")
}

fn cargo_toml() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    fs::read_to_string(path).expect("read Cargo.toml")
}

#[test]
fn default_capability_only_targets_main_window() {
    let capability =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("capabilities/default.json"))
            .expect("read default capability");

    assert!(
        capability.contains("\"windows\": [\"main\"]"),
        "frontend commands should only target Clipline's main window"
    );
    assert!(
        !capability.contains("main-recovery"),
        "recovery windows are intentionally not created or granted frontend command permissions"
    );
}

#[test]
fn renderer_capabilities_match_observed_window_operations() {
    let capability: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("capabilities/default.json"),
        )
        .expect("read default capability"),
    )
    .expect("valid default capability JSON");
    let permissions: Vec<_> = capability["permissions"]
        .as_array()
        .expect("capability permissions")
        .iter()
        .map(|permission| permission.as_str().expect("string permission"))
        .collect();

    assert_eq!(
        permissions,
        [
            "core:default",
            "core:window:allow-toggle-maximize",
            "core:window:allow-close",
            "core:window:allow-start-dragging",
            "autostart:allow-enable",
            "autostart:allow-disable",
            "autostart:allow-is-enabled",
        ]
    );
}

#[test]
fn cloud_pages_and_marker_art_cross_narrow_renderer_boundaries() {
    let cloud = read_ui_js("cloud.js");
    let review = read_ui_js("review-player.js");
    let library = read_ui_js("library.js");

    assert!(
        cloud.contains("await invoke(\"open_cloud_clip\", { remoteClipId: entry.remote_clip_id })")
    );
    assert!(!cloud.contains("invoke(\"open_cloud_clip_url\""));
    assert!(review.contains("PlayerCore.safeMarkerImage"));
    assert!(review.contains("PlayerCore.markerKindConfig"));
    assert!(library.matches("PlayerCore.markerKindConfig").count() >= 2);
}

#[test]
fn native_shell_prevents_duplicate_clipline_instances() {
    let manifest = cargo_toml();
    let app = app_rs();
    let single_instance_plugin = "tauri_plugin_single_instance::init";
    let single_instance = app
        .find(single_instance_plugin)
        .expect("native shell should register the Tauri single-instance plugin");
    let autostart = app
        .find("tauri_plugin_autostart::init")
        .expect("native shell should register autostart");

    assert!(
        manifest.contains("tauri-plugin-single-instance"),
        "Cargo.toml should depend on the single-instance plugin"
    );
    assert!(
        single_instance < autostart,
        "single-instance plugin must be registered before autostart or other shell plugins"
    );
}

#[test]
fn windows_installer_repairs_webview2_with_bootstrapper() {
    let config = tauri_config();

    assert!(
        config.contains("\"minimumWebview2Version\": \"120.0.2210.55\""),
        "Windows 10 installs must repair/update stale WebView2 runtimes before Clipline starts"
    );
    assert!(
        config.contains("\"webviewInstallMode\"")
            && config.contains("\"type\": \"embedBootstrapper\""),
        "the default NSIS installer should embed the small Evergreen bootstrapper instead of bundling the offline WebView2 installer"
    );
}

#[test]
fn configured_main_window_is_not_created_at_startup() {
    let config: serde_json::Value =
        serde_json::from_str(&tauri_config()).expect("tauri.conf.json should parse");
    let create = config
        .pointer("/app/windows/0/create")
        .and_then(|value| value.as_bool());
    assert_eq!(
        create,
        Some(false),
        "main window config must remain a builder template; cold --autostart creates no WebView"
    );
    let app = app_rs();
    assert!(
        app.contains("autostart launch leaving Destroyed shell without webview"),
        "autostart setup must not build or hide a startup webview"
    );
}

#[test]
fn regular_installer_omits_ffmpeg_resources_for_on_demand_runtime() {
    let config: serde_json::Value =
        serde_json::from_str(&tauri_config()).expect("tauri.conf.json should parse");
    let resources = config
        .pointer("/bundle/resources")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert!(
        resources
            .iter()
            .all(|resource| resource.as_str() != Some("ffmpeg/")),
        "regular installer must not embed ffmpeg/; managed runtime is on-demand"
    );
    assert!(
        config.pointer("/build/beforeBundleCommand").is_none(),
        "regular SKU must not run verify-ffmpeg-resource beforeBundleCommand"
    );

    let standalone: serde_json::Value = serde_json::from_str(&tauri_standalone_config())
        .expect("tauri.standalone.conf.json should parse");
    let standalone_resources = standalone
        .pointer("/bundle/resources")
        .and_then(serde_json::Value::as_array)
        .expect("standalone bundle.resources should be listed");
    assert!(
        standalone_resources
            .iter()
            .any(|resource| resource.as_str() == Some("ffmpeg/")),
        "standalone/offline SKU may still bundle ffmpeg beside Fixed Version WebView2"
    );

    let app = app_rs();
    assert!(
        app.contains("configure_bundled_ffmpeg")
            && app.contains("ffmpeg/ffmpeg.exe")
            && app.contains("bundled ffmpeg resource missing"),
        "setup must tolerate a missing bundled ffmpeg resource on the regular SKU"
    );
}

#[test]
fn tauri_config_enforces_a_real_csp() {
    let config: serde_json::Value =
        serde_json::from_str(&tauri_config()).expect("tauri.conf.json should parse");
    let csp = config
        .pointer("/app/security/csp")
        .and_then(serde_json::Value::as_object)
        .expect("tauri config should define a directive-map CSP");

    for directive in [
        "default-src",
        "script-src",
        "style-src",
        "img-src",
        "media-src",
        "connect-src",
        "object-src",
    ] {
        assert!(
            csp.contains_key(directive),
            "CSP should define `{directive}`"
        );
    }

    let img_src = csp
        .get("img-src")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    assert!(img_src.contains("asset:"), "local posters need asset:");
    assert!(img_src.contains("data:"), "embedded game icons need data:");
    assert!(
        img_src.contains("https://assets.ppy.sh"),
        "osu! beatmap covers need assets.ppy.sh"
    );
    assert!(
        img_src.contains("https://ddragon.leagueoflegends.com"),
        "League champion icons need ddragon"
    );

    let connect_src = csp
        .get("connect-src")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    assert!(
        connect_src.contains("ipc:") && connect_src.contains("http://ipc.localhost"),
        "Tauri IPC must stay allowed under CSP"
    );
}

#[test]
fn frontend_reports_webview_readiness_to_native_shell() {
    let app = app_rs();
    let js = main_js();

    assert!(
        app.contains("fn frontend_ready<R: Runtime>(")
            && app.contains("runtime: tauri::State<RuntimeState>")
            && app.contains("startup_warnings: tauri::State<StartupWarnings>")
            && app.contains("readiness: tauri::State<FrontendReadinessState>")
            && app.contains("startup_warnings.snapshot()")
            && app.contains("runtime.durable_recorder_status_for_replay()")
            && app.contains("frontend_ready,"),
        "Rust shell must expose frontend_ready with generation-scoped readiness and durable status/warning replay"
    );
    assert!(
        app.contains("struct FrontendReadinessState")
            && app.contains("fn arm_frontend_ready_watchdog<R: Runtime>(")
            && app.contains("watchdog_should_fire(")
            && app.contains("begin_generation("),
        "native shell must track frontend readiness per window generation"
    );
    assert!(
        js.contains("invoke(\"frontend_ready\")")
            && js.contains("Array.isArray(warnings)")
            && js.contains("warnings.join(\" \")"),
        "main.js must report readiness and render queued startup diagnostics"
    );
}

#[test]
fn manual_recorder_start_rechecks_waiting_state_before_emit() {
    let app = app_rs();
    let start_recording = app
        .split_once("fn start_recording<R: Runtime>")
        .map(|(_, remainder)| remainder)
        .and_then(|remainder| remainder.split_once("fn stop_recording"))
        .map(|(body, _)| body)
        .expect("find RuntimeState::start_recording body");

    assert!(
        start_recording.contains("else if let Some(status) = self.current_waiting_status()")
            && start_recording.contains("app.emit(\"status\", status)"),
        "manual start must re-check durable Waiting state after releasing the runtime lock"
    );
}

#[test]
fn active_recording_status_identifies_the_selected_encoder() {
    let js = main_js();
    let update_status = js_function_body(&js, "updateCaptureStatus");

    assert!(
        js.contains("activeEncoderLabel = s.recording ? String(s.encoder || \"\") : \"\";"),
        "the frontend must retain the backend's active encoder label and clear it when recording stops"
    );
    assert!(
        update_status.contains("Stop recording · ${activeEncoderLabel}")
            && update_status.contains("$(\"rail-status\").title = recordingActive")
            && update_status.contains("? recordingTitle")
            && update_status.contains(": `Start ${source} recording`;"),
        "the active recorder status must assign the concrete encoder selected by Automatic mode to the visible tooltip"
    );
}

#[test]
fn update_dialog_body_can_drag_frameless_window() {
    let html = index_html();
    let css = styles_css();

    let dialog_start = html
        .find("<dialog id=\"update-dialog\"")
        .expect("update dialog exists");
    let dialog_end = html[dialog_start..]
        .find("</dialog>")
        .map(|offset| dialog_start + offset)
        .expect("update dialog closes");
    let dialog = &html[dialog_start..dialog_end];

    assert!(
        dialog.contains("<div class=\"confirm-body update-dialog-drag\" data-tauri-drag-region>"),
        "the update-available modal needs a non-interactive drag region because it appears over the frameless window on launch"
    );
    assert!(
        !dialog
            .split("class=\"confirm-actions\"")
            .nth(1)
            .unwrap_or_default()
            .contains("data-tauri-drag-region"),
        "update dialog action buttons must stay clickable rather than becoming drag handles"
    );
    assert!(
        css.contains(".update-dialog-drag") && css.contains("cursor: move"),
        "the draggable update dialog body should advertise that it can move the window"
    );
}

#[test]
fn elevated_game_hotkey_warning_offers_explicit_restart_once_per_process() {
    let html = index_html();
    let js = main_js();
    let css = styles_css();
    let warning = js_function_body(&js, "maybeWarnElevatedGame");

    for required in [
        "id=\"elevation-dialog\"",
        "id=\"elevation-restart\"",
        "id=\"elevation-cancel\"",
    ] {
        assert!(
            html.contains(required),
            "missing elevated-game UI: {required}"
        );
    }
    assert!(
        js.contains("elevated_hotkeys_blocked")
            && warning.contains("game.process_instance_id")
            && js.contains("warnedElevatedGameProcesses")
            && js.contains("restart_as_administrator")
            && js.contains("restartAsAdministrator")
            && html.contains("Restart as Administrator")
            && html.contains("resets the current rolling replay buffer"),
        "game detection must warn once per process instance and offer an explicit one-launch elevation action"
    );
    assert!(
        css.contains("#elevation-dialog"),
        "the elevation dialog must share the app's in-product modal styling"
    );
    assert!(
        !js.contains("if (ev.target === $(\"elevation-dialog\") && !elevationRestartInFlight)"),
        "backdrop clicks must not dismiss the elevated-game decision"
    );
    assert!(
        js.contains(
            "$(\"elevation-dialog\").addEventListener(\"cancel\", (ev) => {\n  ev.preventDefault();\n});"
        ),
        "Escape must not dismiss the elevated-game decision without an explicit button choice"
    );
}

#[test]
fn administrator_restart_uses_an_exact_parent_handoff() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let app = app_rs();
    let main = fs::read_to_string(root.join("src/main.rs")).expect("read src/main.rs");
    let windows =
        fs::read_to_string(root.join("src/windows/mod.rs")).expect("read src/windows/mod.rs");
    let ui = main_js();

    assert!(
        app.contains("fn restart_as_administrator")
            && app.contains("crate::windows::launch_elevated_after(std::process::id())")
            && app.contains("restart_as_administrator,"),
        "the shell must expose only the current-executable administrator restart"
    );
    assert!(
        main.contains("wait_for_elevation_parent_from_args"),
        "the elevated child must finish the parent handoff before starting Tauri"
    );
    for required in [
        "launch_elevated_after",
        "--clipline-elevated-after",
        "wait_for_elevation_parent_from_args",
        "query_process_identity",
        "\"runas\"",
    ] {
        assert!(
            windows.contains(required),
            "the Windows boundary must retain exact-process handoff primitive {required}"
        );
    }
    assert!(
        ui.contains("restart_as_administrator"),
        "the affirmative UI action must invoke the narrow restart command"
    );
}

#[test]
fn quality_of_life_features_are_wired_through_the_app_shell() {
    let html = index_html();
    let js = main_js();
    let core = player_core_js();
    let css = styles_css();

    assert!(
        html.contains("id=\"set-games-pause-when-empty\"")
            && html.contains("Pause recorder when no game is open")
            && js.contains("pause_when_no_game: false")
            && js.contains("pause_when_no_game: $(\"set-games-pause-when-empty\").checked"),
        "Games settings must persist and expose the opt-in no-game pause policy"
    );
    assert!(
        js.contains("waiting_for_game")
            && js.contains("recorderWaitingForGame")
            && js.contains("\"Waiting\""),
        "recorder status must distinguish policy waiting from a manual stop"
    );
    assert!(
        html.contains("id=\"deck-status-action\"")
            && js.contains("setDeckStatusAction")
            && js.contains("openClip(exportedClip)"),
        "successful exports must offer a direct action to open the exact new clip"
    );
    assert!(
        html.contains("id=\"fullscreen-toggle\"")
            && js.contains("stageFrame.requestFullscreen()")
            && js.contains("document.exitFullscreen()")
            && js.contains("fullscreenchange")
            && core.contains("toggle-fullscreen")
            && css.contains("#stage-frame:fullscreen"),
        "the review stage must expose and synchronize standard fullscreen playback"
    );
}

fn library_rs() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/library.rs");
    fs::read_to_string(path).expect("read src/library.rs")
}

fn cloud_rs() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cloud.rs");
    fs::read_to_string(path).expect("read src/cloud.rs")
}

fn tag_attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let prefix = format!("{name}=\"");
    let start = tag.find(&prefix)? + prefix.len();
    let end = tag[start..].find('"')? + start;
    Some(&tag[start..end])
}

#[test]
fn audio_preview_command_scopes_generated_preview_files() {
    let library = library_rs();

    assert!(
        library.contains("AppHandle")
            && library.contains("allow_audio_preview_asset")
            && library.contains("asset_protocol_scope")
            && library.contains("allow_file(preview"),
        "selected-audio preview MP4s are generated under AppData and must be exact-scoped before the player loads them"
    );
}

#[test]
fn exported_clips_are_scoped_for_immediate_playback() {
    let library = library_rs();
    let export_start = library
        .find("pub async fn export_clip")
        .expect("export_clip command");
    let export_end = library[export_start..]
        .find("\n#[tauri::command]")
        .map(|offset| export_start + offset)
        .unwrap_or(library.len());
    let export_command = &library[export_start..export_end];

    assert!(
        export_command.contains("app: AppHandle<R>")
            && export_command.contains("let scope_root = settings.clips_dir()?")
            && export_command.contains("allow_local_clip_asset(")
            && export_command.contains("Path::new(&exported.path)"),
        "a newly exported MP4 must be exact-scoped before its card can open it without a Library rescan"
    );
}

#[test]
fn audio_sidecar_command_protects_active_media_and_prunes_cache_on_startup() {
    let library = library_rs();
    let app = app_rs();
    assert!(library.contains("pub protected_preview_paths: Vec<String>"));
    assert!(library.contains("prune_audio_preview_cache("));
    assert!(library.contains("touch_audio_preview(final_path)"));
    assert!(app.contains("crate::library::prune_audio_preview_cache_on_startup()"));
}

#[test]
fn audio_sidecar_command_is_the_only_review_audio_generation_contract() {
    let library = library_rs();
    let app = app_rs();
    assert!(library.contains("pub struct PrepareClipAudioSidecarsRequest"));
    assert!(library.contains("pub protected_preview_paths: Vec<String>"));
    assert!(library.contains("pub struct PreparedClipAudioSidecar"));
    assert!(library.contains("pub audio_track_id: String"));
    assert!(library.contains("pub async fn prepare_clip_audio_sidecars"));
    assert!(app.contains("crate::library::prepare_clip_audio_sidecars"));
}

#[test]
fn legacy_audio_preview_code_is_absent() {
    let library = library_rs();
    let app = app_rs();
    let review = read_ui_js("review-player.js");
    for legacy in [
        "pub struct AudioPreviewRequest",
        "pub protected_preview_path: Option<String>",
        "pub async fn preview_clip_audio_tracks",
        "fn preview_clip_audio_tracks_file",
        "fn preview_clip_audio_tracks_file_with_mixer",
        "fn write_audio_preview",
        "fn audio_preview_path(",
        "audio-preview-mix-v4",
        "fn mix_audio_tracks_with_ffmpeg",
    ] {
        assert!(
            !library.contains(legacy),
            "legacy preview code remains: `{legacy}`"
        );
    }
    assert!(!app.contains("crate::library::preview_clip_audio_tracks"));
    assert!(!review.contains("invoke(\"preview_clip_audio_tracks\""));
    assert!(!library.contains("amix=inputs="));
    assert!(library.contains("remux_with_mixed_audio_track"));
}

#[test]
fn review_player_owns_all_controls() {
    let html = index_html();

    let video_start = html.find("<video").expect("video element exists");
    let video_end = html[video_start..]
        .find('>')
        .map(|offset| video_start + offset)
        .expect("video element closes");
    assert!(
        !html[video_start..=video_end].contains("controls"),
        "the review player must not expose native browser video controls"
    );

    for required in [
        "id=\"play-toggle\"",
        "id=\"seek-back\"",
        "id=\"seek-forward\"",
        "id=\"prev-marker\"",
        "id=\"next-marker\"",
        "id=\"marker-count\"",
        "id=\"timeline\"",
        "id=\"trim-band\"",
        "id=\"handle-in\"",
        "id=\"handle-out\"",
        "id=\"time-readout\"",
        "id=\"rate-select\"",
        "id=\"mute-toggle\"",
        "id=\"fullscreen-toggle\"",
        "id=\"volume-slider\"",
        "id=\"export-clip\"",
        "id=\"trim-summary\"",
        "id=\"keys-dialog\"",
        "id=\"keys-close\"",
        "id=\"delete-clip\"",
        "id=\"ruler\"",
        "id=\"overview\"",
        "id=\"overview-trim\"",
        "id=\"overview-markers\"",
        "id=\"overview-playhead\"",
        "id=\"overview-window\"",
        "id=\"overview-window-l\"",
        "id=\"overview-window-r\"",
        "id=\"game-event-rail\"",
        "id=\"game-event-rail-title\"",
        "id=\"game-event-rail-summary\"",
        "id=\"game-event-rail-toggle\"",
        "id=\"game-event-list\"",
        "id=\"game-metadata-panel\"",
        "id=\"game-metadata-fields\"",
        "id=\"zoom-out\"",
        "id=\"zoom-fit\"",
        "id=\"zoom-in\"",
        "id=\"snap-toggle\"",
        "id=\"trim-mode-toggle\"",
        "id=\"audio-track-panel\"",
        "id=\"audio-track-summary\"",
        "id=\"audio-track-list\"",
        "id=\"open-folder\"",
        "id=\"stage-frame\"",
        "id=\"copy-clip\"",
        "id=\"stage-overlay\"",
        "id=\"memory-usage\"",
        "id=\"memory-children\"",
        "id=\"rail-hotkey\"",
        "id=\"rail-dot\"",
        "id=\"rail-status-text\"",
        "id=\"rail-status\"",
        "id=\"rail-status\" title=\"Stop recording\" aria-pressed=\"true\"",
        "id=\"rail-save\"",
        "id=\"rail-library-status\"",
        "id=\"rail-clips-count\"",
        "id=\"rail-profile\"",
        "id=\"rail-profile-avatar\"",
        "id=\"rail-profile-name\"",
        "id=\"rail-settings\"",
        "id=\"confirm-dialog\"",
        "id=\"confirm-accept\"",
        "id=\"confirm-cancel\"",
        "id=\"quit-dialog\"",
        "id=\"quit-accept\"",
        "id=\"quit-cancel\"",
        "id=\"update-dialog\"",
        "id=\"update-install\"",
        "id=\"update-cancel\"",
        "id=\"update-dialog-title\"",
        "id=\"update-dialog-body\"",
        "id=\"settings-page\"",
        "id=\"settings-tabs\"",
        "id=\"set-open-on-startup\"",
        "id=\"set-close-to-tray\"",
        "id=\"set-minimize-to-tray\"",
        "id=\"set-legacy-timeline-editor\"",
        "id=\"set-update-channel\"",
        "id=\"check-updates\"",
        "id=\"update-status\"",
        "id=\"set-capture\"",
        "id=\"set-backend\"",
        "id=\"backend-summary\"",
        "id=\"set-output-enabled\"",
        "id=\"set-audio-split-output\"",
        "id=\"set-output-device\"",
        "id=\"set-output-volume\"",
        "id=\"output-volume-summary\"",
        "id=\"set-mic-enabled\"",
        "id=\"set-mic-device\"",
        "id=\"set-mic-volume\"",
        "id=\"mic-volume-summary\"",
        "id=\"set-mic-mono\"",
        "id=\"test-mic\"",
        "id=\"mic-meter-fill\"",
        "id=\"mic-test-status\"",
        "id=\"capture-region-editor\"",
        "id=\"display-map\"",
        "id=\"display-map-inner\"",
        "id=\"region-box\"",
        "id=\"region-display-label\"",
        "id=\"set-region-width\"",
        "id=\"set-region-height\"",
        "id=\"set-region-x\"",
        "id=\"set-region-y\"",
        "id=\"capture-region-menu\"",
        "id=\"region-align-menu\"",
        "id=\"region-display-menu\"",
        "id=\"clip-context-menu\"",
        "id=\"clip-menu-play\"",
        "id=\"clip-menu-open-cloud-page\"",
        "id=\"clip-menu-copy-cloud-link\"",
        "id=\"clip-menu-upload\"",
        "id=\"clip-menu-rename\"",
        "id=\"clip-menu-delete\"",
        "id=\"clip-title-display\"",
        "id=\"rename-clip\"",
        "id=\"clip-title-edit\"",
        "id=\"rename-input\"",
        "id=\"rename-save\"",
        "id=\"rename-cancel\"",
        "id=\"upload-dialog\"",
        "id=\"upload-title\"",
        "id=\"upload-description\"",
        "id=\"upload-visibility\"",
        "id=\"upload-audio-section\"",
        "id=\"upload-audio-list\"",
        "id=\"upload-confirm\"",
        "id=\"upload-cancel\"",
        "id=\"upload-dialog-status\"",
        "id=\"set-buffer\"",
        "id=\"set-encoder\"",
        "id=\"encoder-summary\"",
        "id=\"set-output-resolution\"",
        "id=\"output-resolution-summary\"",
        "id=\"set-replay\"",
        "id=\"replay-summary\"",
        "id=\"replay-scale\"",
        "id=\"set-bitrate\"",
        "id=\"quality-summary\"",
        "id=\"quality-scale\"",
        "id=\"set-fps\"",
        "id=\"fps-summary\"",
        "id=\"fps-scale\"",
        "id=\"recording-mode-basic\"",
        "id=\"recording-mode-advanced\"",
        "id=\"recording-basic-fields\"",
        "id=\"recording-advanced-fields\"",
        "id=\"set-output-width\"",
        "id=\"set-output-height\"",
        "id=\"set-custom-bitrate\"",
        "id=\"set-custom-fps\"",
        "id=\"set-media-dir\"",
        "id=\"choose-media-folder\"",
        "id=\"set-quota\"",
        "id=\"set-replay-disk-enabled\"",
        "id=\"replay-disk-fields\"",
        "id=\"set-replay-disk-dir\"",
        "id=\"choose-replay-cache-folder\"",
        "id=\"set-replay-disk-quota\"",
        "id=\"replay-disk-estimate\"",
        "id=\"set-replay-disk-ack\"",
        "data-tab=\"cloud\"",
        "data-section=\"cloud\"",
        "id=\"cloud-connect-fields\"",
        "id=\"cloud-host-url\"",
        "id=\"cloud-username\"",
        "id=\"cloud-password\"",
        "id=\"cloud-http-warning\"",
        "id=\"cloud-connect\"",
        "id=\"cloud-disconnect\"",
        "id=\"cloud-connect-status\"",
        "id=\"cloud-connection-status\"",
        "id=\"cloud-default-visibility\"",
        "id=\"cloud-delete-local-after-upload\"",
        "id=\"cloud-auto-upload-rules\"",
        "data-tab=\"games\"",
        "data-section=\"games\"",
        "id=\"set-games-auto-detect\"",
        "id=\"supported-games\"",
        "id=\"custom-games\"",
        "id=\"detect-games\"",
        "id=\"add-custom-game\"",
        "id=\"detected-games-dialog\"",
        "id=\"detected-games-list\"",
        "id=\"add-detected-games\"",
        "id=\"cancel-detected-games\"",
        "id=\"game-window-picker-dialog\"",
        "id=\"refresh-game-windows\"",
        "id=\"game-window-list\"",
        "id=\"game-detection-status\"",
        "id=\"set-hotkey\"",
        "id=\"settings-save\"",
        "id=\"settings-close\"",
    ] {
        assert!(
            html.contains(required),
            "review player is missing required control {required}"
        );
    }

    assert!(
        html.contains("value=\"display_region\""),
        "capture target must expose the display_region mode"
    );
    assert!(
        html.contains("Experimental")
            && html.contains("set-audio-split-output")
            && main_js().contains("split_output_by_process")
            && main_js().contains("split_output_by_process: false"),
        "capture settings must expose and persist the experimental audio-splitting toggle"
    );
    assert!(
        html.contains("Close to Tray")
            && html.contains("Minimize to Tray")
            && html.contains("Legacy timeline editor")
            && html.contains("Updates")
            && html.contains("value=\"stable\" disabled")
            && main_js().contains("close_to_tray")
            && main_js().contains("minimize_to_tray")
            && main_js().contains("legacy_timeline_editor")
            && main_js().contains("update_channel")
            && main_js().contains("check_for_updates")
            && main_js().contains("install_update")
            && main_js().contains("function updateUpToDateStatus(update)")
            && main_js().contains("update.current_version")
            && main_js().contains("update.status || updateUpToDateStatus(update)")
            && main_js().contains("checkForUpdates({ manual: false })")
            && app_rs().contains("tauri_plugin_updater::Builder::new().build()")
            && main_js().contains("minimize_main_window"),
        "general settings must expose and persist tray close/minimize/preview/update behavior"
    );
    assert!(
        main_js().contains("function setSimpleTrimMode(active)")
            && main_js().contains("function applyTimelineEditorPreference()")
            && main_js().contains("quickTrimRange(")
            && styles_css().contains(".deck.simple-timeline")
            && styles_css().contains("#trim-mode-toggle.active")
            && styles_css().contains(".deck.legacy-timeline"),
        "review timeline must default to simple trim mode while preserving the legacy editor mode"
    );
    assert!(
        main_js().contains("requestWindowClose")
            && main_js().contains("confirmQuit")
            && main_js().contains("close_to_tray === false")
            && styles_css().contains("#quit-dialog"),
        "the window close button must confirm before quitting when Close to Tray is disabled"
    );
    assert!(
        !html.contains(">primary monitor<")
            && main_js().contains("renderCaptureTargetSelect")
            && main_js().contains("displayCaptureValue")
            && main_js().contains("display:")
            && html.contains(">SET REGION<")
            && main_js().find("displayCaptureValue").unwrap()
                < main_js().find("region.value = \"display_region\"").unwrap(),
        "capture target must list available displays before the display-region option"
    );
    assert!(
        !html.contains("value=\"window_title\"") && !html.contains("id=\"set-window\""),
        "manual window-title capture was replaced by custom game detection"
    );
    assert!(
        html.contains("data-replay-preset=\"30\"")
            && html.contains("data-replay-preset=\"60\"")
            && html.contains("data-replay-preset=\"120\""),
        "recording tab must expose quick save-length presets up to two minutes"
    );
    assert!(
        !html.contains("data-replay-preset=\"300\""),
        "save length must not expose presets beyond two minutes"
    );
    let replay_start = html
        .find("id=\"set-replay\"")
        .expect("replay control exists");
    let replay_tag_end = html[replay_start..]
        .find('>')
        .map(|offset| replay_start + offset)
        .expect("replay control tag closes");
    assert!(
        html[replay_start..=replay_tag_end].contains("max=\"120\""),
        "save length slider must stop at two minutes"
    );
    let fps_start = html.find("id=\"set-fps\"").expect("fps control exists");
    let fps_tag_end = html[fps_start..]
        .find('>')
        .map(|offset| fps_start + offset)
        .expect("fps control tag closes");
    assert!(
        html[fps_start..=fps_tag_end].contains("type=\"range\""),
        "smoothness must be a slider, not a dropdown"
    );
    assert!(
        html.contains("data-settings-key=\"advanced_recording\"")
            && main_js().contains("advanced_recording")
            && main_js().contains("syncRecordingModeFields"),
        "recording tab must expose and persist advanced exact recording controls"
    );
    assert!(
        html.contains("id=\"hotkey-status\""),
        "hotkeys page must expose recorder status text"
    );
    let hotkey_start = html.find("id=\"set-hotkey\"").expect("hotkey input exists");
    let hotkey_tag_end = html[hotkey_start..]
        .find('>')
        .map(|offset| hotkey_start + offset)
        .expect("hotkey input tag closes");
    assert!(
        html[hotkey_start..=hotkey_tag_end].contains("readonly"),
        "hotkey input must record shortcuts instead of accepting free text"
    );
    let media_dir_start = html
        .find("id=\"set-media-dir\"")
        .expect("media folder input exists");
    let media_dir_tag_end = html[media_dir_start..]
        .find('>')
        .map(|offset| media_dir_start + offset)
        .expect("media folder input tag closes");
    assert!(
        html[media_dir_start..=media_dir_tag_end].contains("readonly"),
        "media folder should be chosen with the native folder picker"
    );
    assert!(
        html.contains("Choose Folder"),
        "storage settings must expose a native-folder-picker action"
    );
    assert!(
        html.contains("Disk replay buffer (advanced)")
            && html.contains("Only turn this on if you know what you're doing")
            && html.contains("can add significant SSD wear")
            && html.contains(
                "I understand this continuously writes to disk and can shorten SSD life."
            ),
        "disk replay buffer settings must carry explicit advanced SSD-wear warnings"
    );
    assert!(
        html.contains(">Cloud<")
            && html.contains("I understand my password will be sent without TLS to")
            && html.contains("id=\"cloud-http-confirm\"")
            && html.contains("id=\"cloud-http-origin\"")
            && main_js().contains("cloud_connect")
            && main_js().contains("cloud_disconnect")
            && main_js().contains("function cloudInsecureHttpOrigin(")
            && main_js().contains("CloudCore.plainHttpConfirmed(")
            && main_js().contains("plain_http_confirmed: plainHttpConfirmed")
            && main_js().contains("if (plainHttpOrigin && !plainHttpConfirmed)")
            && main_js().contains("confirm.checked = false")
            && !main_js().contains("plain_http_confirmed: cloudHostUsesInsecureHttp()")
            && main_js().contains(
                "$(\"cloud-host-url\").addEventListener(\"input\", syncCloudHttpWarning)"
            )
            && main_js().contains(
                "$(\"cloud-host-url\").value = connected ? \"\" : cloud.host_url || \"\""
            )
            && main_js().contains(
                "$(\"cloud-username\").value = connected ? \"\" : cloud.connected_username || \"\""
            )
            && main_js().contains("$(\"cloud-connect-fields\").hidden = connected")
            && main_js().contains("$(\"cloud-connect\").hidden = connected")
            && main_js().contains("$(\"cloud-disconnect\").hidden = !connected")
            && main_js().contains("upload_clip_to_cloud")
            && main_js().contains("function openUploadDialog(clip)")
            && main_js().contains("title: request.title || clipUploadDefaultTitle(clip)")
            && main_js().contains("visibility: request.visibility || cloudSettings().default_visibility || \"private\"")
            && html.contains("id=\"upload-dialog\"")
            && html.contains("id=\"upload-title\"")
            && html.contains("id=\"upload-description\"")
            && html.contains("maxlength=\"5000\"")
            && !html.contains("Not supported by Clipline Cloud yet")
            && !main_js().contains("Descriptions are not supported by Clipline Cloud yet.")
            && main_js().contains("description: request.description || null")
            && html.contains("id=\"upload-visibility\"")
            && html.contains("id=\"upload-audio-section\"")
            && html.contains("id=\"upload-audio-list\"")
            && main_js().contains("function clipAudioTracks(clip = currentClip)")
            && main_js().contains("function renderAudioTrackPanel()")
            && main_js().contains("function requestSelectedAudioPreview()")
            && main_js().contains("prepare_clip_audio_sidecars")
            && main_js().contains("function renderUploadAudioTracks(clip = uploadDialogClip)")
            && main_js().contains("audioTrackIds: request.audioTrackIds || null")
            && !main_js().contains("video.audioTracks")
            && !main_js().contains("applyNativeAudioTrackSelection")
            && main_js().contains("audio-track-label")
            && styles_css().contains(".audio-track-panel")
            && styles_css().contains(".audio-track-row")
            && styles_css().contains(".audio-track-label")
            && styles_css().contains(".upload-audio-section[hidden] { display: none; }")
            && main_js().contains("listen(\"cloud-upload-progress\"")
            && main_js().contains("navigator.clipboard.writeText(record.remote_url)")
            && main_js().contains("syncUploadClipButton();")
            && main_js().contains("Connect Clipline Cloud before uploading.")
            && main_js().contains("function clipCloudVisibility(record)")
            && main_js().contains("CLOUD_VISIBILITY_ICONS")
            && main_js().contains("clip-cloud-visibility")
            && !main_js().contains(" · cloud:")
            && app_rs().contains("crate::cloud::cloud_connect")
            && app_rs().contains("crate::cloud::upload_clip_to_cloud")
            && app_rs().contains("crate::cloud::sync_cloud_clip_status")
            && app_rs().contains("crate::library::prepare_clip_audio_sidecars")
            && main_js().contains("sync_cloud_clip_status")
            && main_js().contains("result.truncated")
            && main_js().contains("first 10,000 unique cloud clips")
            && styles_css().contains(".cloud-connect-grid")
            && styles_css().contains(".cloud-connect-fields")
            && styles_css().contains(".cloud-connect-fields[hidden] { display: none; }")
            && styles_css().contains(".cloud-http-warning")
            && styles_css().contains(".cloud-http-warning input")
            && styles_css().contains(".cloud-http-warning span")
            && styles_css().contains("word-break: break-all")
            && styles_css().contains(".cloud-http-warning[hidden] { display: none; }")
            && styles_css().contains(".clip-cloud-visibility.public")
            && styles_css().contains(".clip-cloud-visibility.unlisted")
            && styles_css().contains(".clip-cloud-visibility.private")
            && styles_css().contains(".clip .clip-title")
            && styles_css().contains(".review-head .clip-title")
            && styles_css().contains("#upload-dialog")
            && html.contains("id=\"upload-clip\"")
            && styles_css().contains(".review-actions .icon-button.uploaded"),
        "cloud settings, upload controls, and per-clip visibility icons must stay wired"
    );
    assert!(
        html.contains(">Games<") && html.contains("Add Custom Game"),
        "settings must expose the Games tab and custom game action"
    );
    assert!(
        html.contains(">Supported games<")
            && html.contains("loading supported games...")
            && !html.contains(">Game plugins<")
            && !html.contains("loading game plugins..."),
        "Settings > Games must name built-in integrations as supported games"
    );
    assert!(
        main_js().contains("gameRecordingModeControl")
            && main_js().contains("custom-game-recording-mode")
            && main_js().contains("recording_mode")
            && main_js().contains("replays_only")
            && main_js().contains("full_session")
            && styles_css().contains(".custom-game-mode"),
        "custom games must expose and persist per-game recording mode choices"
    );
    assert!(
        styles_css().contains("max-height: clamp(180px, calc(100vh - 360px), 460px);")
            && styles_css().contains("overflow-y: auto;")
            && styles_css().contains(
                "grid-template-columns: auto auto minmax(0, 1fr) minmax(220px, 320px) auto;"
            )
            && styles_css().contains(".custom-game-mode {\n  grid-column: auto;")
            && main_js().contains(
                "row.append(enabled, icon, meta, gameRecordingModeControl(game, index), remove);"
            ),
        "custom games list must scroll independently and keep recording mode on the right side"
    );
    assert!(
        main_js().contains("await invoke(\"list_game_plugins\")")
            && main_js().contains("renderGamePlugins")
            && main_js().contains("gamePluginSettings")
            && main_js().contains("plugin.presentation")
            && main_js().contains("games.plugins")
            && main_js().contains("dataset.gamePluginEnabled")
            && main_js().contains("game-plugin-mode-")
            && main_js().contains("normalizeGamePluginId")
            && main_js().contains("Takes priority over matching custom games.")
            && !main_js().contains("check_game_plugin_package")
            && !main_js().contains("update_game_plugin_package")
            && !main_js().contains("reinstall_game_plugin_package")
            && !main_js().contains("reset_game_plugin_to_seed")
            && !main_js().contains("plugin.latest_version")
            && !main_js().contains("plugin.latest_source_label")
            && !main_js().contains("dataset.gamePluginAction")
            && !styles_css().contains(".game-plugin-actions")
            && styles_css().contains(".game-profile-mode"),
        "supported games must render from backend profiles without package install/update actions"
    );
    assert!(
        main_js().contains("function defaultGamePluginReviewSettings")
            && main_js().contains("function normalizeGamePluginReviewSettings")
            && main_js().contains("plugin.default_review")
            && main_js().contains("function renderGamePluginSettingsButton")
            && main_js().contains("function showGamePluginSettingsDialog")
            && main_js().contains("function hideGamePluginSettingsDialog")
            && main_js().contains("function renderGamePluginSettingsDialog")
            && main_js().contains("function renderGamePluginSettingsGeneralTab")
            && main_js().contains("function renderGamePluginSettingsMatchEventsTab")
            && main_js().contains("function renderGamePluginSettingsTimelineMarkersTab")
            && main_js().contains("function renderOsuAccountSettingsTab")
            && main_js().contains("function renderOsuPlaysSettingsTab")
            && main_js().contains("const GAME_REVIEW_OPTION_GROUPS")
            && main_js().contains("const GAME_PLUGIN_SETTINGS_TAB_DEFINITIONS")
            && main_js().contains("function renderGamePluginOptionGroup")
            && main_js().contains("Your events")
            && main_js().contains("Team fights")
            && main_js().contains("Map events")
            && main_js().contains("Your markers")
            && main_js().contains("Map markers")
            && main_js().contains("Show League match details")
            && main_js().contains("Use your own osu! OAuth app")
            && main_js().contains("Test osu! API connection")
            && main_js().contains(
                "Recent submitted plays are fetched after a full-session recording is saved."
            )
            && main_js().contains("Some plays may be missing")
            && !main_js().contains("Enhanced review view")
            && main_js().contains("data-game-plugin-review-enabled")
            && main_js().contains("data-game-plugin-review-setting")
            && main_js().contains("match_events")
            && main_js().contains("timeline_markers")
            && main_js().contains("osu_account")
            && main_js().contains("osu_plays")
            && main_js().contains("team_kills")
            && main_js().contains("enemy_deaths")
            && main_js().contains("PlayerCore.reviewMatchEventMarkers")
            && main_js().contains("PlayerCore.reviewTimelineMarkers")
            && index_html().contains("id=\"game-plugin-settings-dialog\"")
            && index_html().contains("id=\"game-plugin-settings-tabs\"")
            && index_html().contains("General")
            && index_html().contains("Match events")
            && index_html().contains("Timeline markers")
            && index_html().contains("Account")
            && index_html().contains("Plays")
            && main_js().matches("game-plugin-settings-dialog").count() >= 2
            && styles_css().contains(".game-profile-settings")
            && styles_css().contains(".game-plugin-settings-dialog")
            && styles_css().contains(".game-plugin-settings-tabs")
            && styles_css().contains(".game-plugin-settings-tabs .tab[hidden]")
            && styles_css().contains(".game-plugin-settings-body")
            && styles_css().contains(".game-review-master-card")
            && styles_css().contains(".game-review-option-group")
            && styles_css().contains(".game-review-option-list")
            && styles_css().contains(".osu-account-panel")
            && styles_css().contains(".osu-play-settings-list")
            && styles_css().contains("align-items: start")
            && styles_css().contains("align-content: start")
            && !main_js().contains("is_timeline_marker"),
        "supported games must expose persisted League match detail controls in the settings dialog"
    );
    let settings = read_ui_js("settings.js");
    let render_games = js_function_body(&settings, "renderGamePlugins");
    assert!(
        render_games.contains("empty.textContent = \"no supported games available\"")
            && !render_games.contains("not installed")
            && !render_games.contains("repair available")
            && !render_games.contains("Package is current"),
        "Settings > Games copy should describe built-in supported games, not installable packages"
    );
    assert!(
        !app_rs().contains("check_game_plugin_package")
            && !app_rs().contains("update_game_plugin_package")
            && !app_rs().contains("reinstall_game_plugin_package")
            && !app_rs().contains("reset_game_plugin_to_seed")
            && !app_rs().contains("seed_bundled_plugins")
            && !app_rs().contains("plugin_install_root"),
        "Clipline should not expose installable game package commands"
    );
    assert!(
        main_js().contains("function pluginPresentationForClip(clip)")
            && main_js().contains("function clipGalleryCardPreview(clip, kind, fallbackTitle)")
            && main_js().contains("function renderGameEventRail")
            && main_js().contains("gameEventRailItem")
            && main_js().contains("game-event-duel")
            && main_js().contains("game-event-actor-event")
            && main_js().contains("game-event-objective-icon")
            && !main_js().contains("game-event-objective-label")
            && main_js().contains("game-event-portrait")
            && main_js().contains("game-event-kind-icon")
            && main_js().contains("function syncGameEventRail")
            && main_js().contains("function setGameEventRailCollapsed")
            && main_js().contains("function selectGameEvent")
            && main_js().contains("function clearGameEventSelection")
            && main_js().contains("gameEventActiveIndex")
            && main_js().contains("keepGameEventSelection")
            && main_js().contains("gameEventRailCollapsed")
            && main_js().contains("event-rail-collapsed")
            && main_js().contains("game-event-rail-toggle\").addEventListener(\"click\"")
            && main_js().contains("setGameEventRailCollapsed(!gameEventRailCollapsed)")
            && main_js().contains("syncGameEventRail(video.currentTime || 0, { force: true })")
            && main_js().contains("function renderGameMetadataPanel")
            && main_js().contains("function clipPlays(clip = currentClip)")
            && main_js().contains("function renderPlayBlocks")
            && main_js().contains("function renderGamePlayRail")
            && main_js().contains("function syncGamePlayRail")
            && main_js().contains("function renderMetadataIconList(field)")
            && main_js().contains("field.type === \"summoner_spells\" || field.type === \"item_build\"")
            && main_js().contains("presentation.event_rail")
            && main_js().contains("presentation.metadata_panel")
            && main_js().contains("playerSummaryFields")
            && main_js().contains("galleryCardPreview")
            && main_js().contains("playBlocks(")
            && main_js().contains("playRailItem(")
            && main_js().contains("playActiveIndex")
            && main_js().contains("data-game-play-index")
            && main_js().contains("Set plays")
            && main_js().contains("data_dragon: presentation && presentation.data_dragon")
            && main_js().contains("data-game-event-index")
            && player_core_js().contains("gallery.summary === \"osu_set_plays\"")
            && player_core_js().contains("titlePolicy === \"osu_session_summary\"")
            && player_core_js().contains("gallery.summary === \"player_summary_kda\"")
            && player_core_js().contains("titlePolicy === \"summary_for_full_session\"")
            && player_core_js().contains("playerSummaryStatsLabel")
            && player_core_js().contains("type === \"cs_per_min\"")
            && main_js().contains("const cardPreview = clipGalleryCardPreview(c, kind, fallbackTitle)")
            && main_js().contains("cardPreview.titleSource === \"summary\"")
            && main_js().contains("cardPreview.icon")
            && styles_css().contains(".card-game-ico.portrait")
            && styles_css().contains(".game-metadata-icons.summoner_spells")
            && styles_css().contains(".game-metadata-icons.item_build")
            && player_core_js().contains("const dataDragonAsset =")
            && !player_core_js().contains("dataDragonChampionSquareAsset")
            && !player_core_js().contains("dataDragonSummonerSpellAsset")
            && !player_core_js().contains("dataDragonItemAsset")
            && player_core_js().contains("const clipName = clip && typeof clip.name === \"string\" ? clip.name.trim() : \"\"")
            && player_core_js().contains("const customTitle = clip && typeof clip.title === \"string\" ? clip.title.trim() : \"\"")
            && player_core_js().contains("const clipDisplayTitle = customTitle || clipName.replace")
            && player_core_js().contains("titlePolicy === \"clip\" || (titlePolicy === \"osu_session_summary\" && kind !== \"session\")")
            && player_core_js().contains("const clipTitle = usesClipTitle && clipDisplayTitle ? clipDisplayTitle : fallback")
            && player_core_js().contains("const markerRailConfig =")
            && main_js().contains("detail.className = \"game-meta\"")
            && main_js().contains("if (cardPreview.summary && !cardTitleUsesSummary)")
            && main_js().contains("const infoParts = []")
            && main_js().contains(
                "if (Number.isFinite(c.duration_s)) infoParts.push(fmtDur(c.duration_s))"
            )
            && main_js().contains("if (!cardPreview.summary && digest) infoParts.push(digest)")
            && !main_js().contains("LEAGUE_OF_LEGENDS_ID")
            && !main_js().contains("isLeagueClip")
            && !main_js().contains("function renderGamePanel")
            && index_html().contains("aria-controls=\"game-event-list\"")
            && index_html().contains("id=\"game-play-rail\"")
            && index_html().contains("id=\"game-play-list\"")
            && index_html().contains("id=\"play-block-layer\"")
            && styles_css().contains(".play-block-layer")
            && styles_css().contains(".play-block")
            && styles_css().contains(".game-play-rail")
            && styles_css().contains(".review-body.has-event-rail.event-rail-collapsed")
            && styles_css().contains(".game-event-rail-tab")
            && styles_css().contains(".game-event-row-friendly")
            && styles_css().contains(".game-event-row-enemy")
            && styles_css().contains(".game-event-rail ol button.game-event-row-friendly")
            && styles_css().contains(".game-event-rail ol button.game-event-row-enemy")
            && styles_css().contains(".game-event-rail .game-event-objective-icon")
            && styles_css().contains("grid-column: 4;")
            && styles_css().contains("grid-template-columns: 38px minmax(46px, 1fr) 34px minmax(46px, 1fr);")
            && styles_css().contains(".game-event-rail ol button.game-event-duel .game-event-kind-icon")
            && styles_css().contains("width: 34px;\n  height: 34px;\n  overflow: visible;")
            && !styles_css().contains("align-self: start;\n  margin-top: 7px;")
            && !styles_css().contains(".game-event-rail ol button.marker-kill .game-event-kind-icon img")
            && !styles_css().contains(".game-event-rail ol button.marker-death .game-event-kind-icon img")
            && styles_css().contains("border: 0;\n  border-radius: 0;\n  background: transparent;")
            && styles_css().contains("filter:\n    drop-shadow(1px 0 0 rgba(var(--scrim-a-rgb), 0.9))")
            && styles_css().contains(".game-event-name")
            && styles_css().contains(".game-event-rail:hover .game-event-rail-tab")
            && styles_css().contains("--game-event-rail-pad: 10px;")
            && styles_css().contains("left: 0;")
            && !styles_css().contains("left: var(--game-event-rail-pad);")
            && styles_css().contains("top: 50%;")
            && styles_css().contains("transform: translate(-100%, -50%)")
            && styles_css().contains(".game-event-rail::before")
            && styles_css().contains("left: -34px;")
            && styles_css().contains("height: 72px;")
            && styles_css().contains("pointer-events: auto;")
            && styles_css().contains("transition: opacity 120ms ease, background 120ms ease;")
            && styles_css().contains(".game-event-rail-tab:active")
            && !styles_css().contains("translate(calc(-100% + 8px), -50%)")
            && !styles_css().contains("transition: opacity 120ms ease, transform")
            && styles_css().contains("padding: var(--game-event-rail-pad);")
            && styles_css().contains(".game-event-rail.is-collapsed")
            && index_html().contains(
                "<svg class=\"i-collapse\" viewBox=\"0 0 24 24\"><path d=\"M8.6 16.6 10 18l6-6-6-6-1.4 1.4 4.6 4.6-4.6 4.6z\"/></svg>"
            )
            && index_html().contains(
                "<svg class=\"i-expand\" viewBox=\"0 0 24 24\"><path d=\"M15.4 7.4 14 6l-6 6 6 6 1.4-1.4L10.8 12l4.6-4.6z\"/></svg>"
            )
            && styles_css().contains(".clip .game-meta"),
        "plugin-driven game rows must keep League title/KDA behavior and render right-side events plus declarative bottom metadata"
    );
    assert!(
        !index_html().contains("game-profile planned"),
        "planned game cards should not sit in static HTML where renderGamePlugins wipes them"
    );

    // Settings is a page in the main pane now, not a sidebar fold.
    assert!(
        !html.contains("settings-fold"),
        "the sidebar settings fold was replaced by #settings-page"
    );
    // Reversed (2026-06-12, PR #5): the footer now carries an explicit Close
    // button after Save, replacing the earlier "close only from the rail" rule.
    let settings_save = html
        .find("id=\"settings-save\"")
        .expect("settings save button");
    let settings_close = html
        .find("id=\"settings-close\"")
        .expect("settings close button");
    assert!(
        settings_save < settings_close,
        "Close must come after Save in the footer markup"
    );

    // Removed on purpose (2026-06-12): clicking the active library row again
    // closes the clip; the new copy affordance must not revive the old path-only id.
    for gone in [
        "id=\"copy-path\"",
        "id=\"close-review\"",
        "id=\"focus-toggle\"",
    ] {
        assert!(
            !html.contains(gone),
            "{gone} was removed from the header — do not reintroduce it"
        );
    }
    let upload_clip = html.find("id=\"upload-clip\"").expect("upload clip button");
    let open_folder = html.find("id=\"open-folder\"").expect("open folder button");
    let copy_clip = html.find("id=\"copy-clip\"").expect("copy clip button");
    let delete_clip = html.find("id=\"delete-clip\"").expect("delete clip button");
    assert!(
        upload_clip < open_folder,
        "upload button must sit immediately left of Open Folder in the review header"
    );
    assert!(
        open_folder < copy_clip && copy_clip < delete_clip,
        "copy clip must sit beside Open Folder before the destructive action"
    );

    // Conventional ordering: transport glued to the stage, timeline below it.
    let transport = html.find("id=\"play-toggle\"").expect("play toggle");
    let timeline = html.find("id=\"timeline\"").expect("timeline");
    assert!(
        transport < timeline,
        "transport row must precede the timeline in the deck"
    );
    assert!(
        styles_css().contains(".stage-frame")
            && styles_css().contains("object-fit: contain")
            && main_js().contains("updateStageFrame"),
        "the review stage must size an aspect-locked frame around the video"
    );
    // Icon buttons carry SVG icons; text labels are a regression.
    for id in [
        "id=\"play-toggle\"",
        "id=\"seek-back\"",
        "id=\"seek-forward\"",
        "id=\"prev-marker\"",
        "id=\"next-marker\"",
        "id=\"mute-toggle\"",
        "id=\"fullscreen-toggle\"",
        "id=\"upload-clip\"",
        "id=\"open-folder\"",
        "id=\"copy-clip\"",
        "id=\"rail-save\"",
        "id=\"rail-settings\"",
        "id=\"delete-clip\"",
        "id=\"export-clip\"",
        "id=\"zoom-out\"",
        "id=\"zoom-fit\"",
        "id=\"zoom-in\"",
        "id=\"snap-toggle\"",
    ] {
        let start = html.find(id).expect("transport button exists");
        let body_end = html[start..]
            .find("</button>")
            .map(|o| start + o)
            .expect("button closes");
        assert!(
            html[start..body_end].contains("<svg"),
            "{id} must render an SVG icon, not a text label"
        );
    }
}

#[test]
fn custom_game_ids_use_a_reserved_namespace_and_migrated_icons_do_not_become_plugins() {
    let js = main_js();

    assert!(
        js.contains("return `custom-${slug}-${Date.now()}`")
            && js.contains("for (const plugin of gamePlugins) usedIds.add(plugin.id)"),
        "new custom games must stay in the custom namespace and reserve catalog ids"
    );
    assert!(
        js.contains("function customGameForRecordedGame(recordedGame)")
            && js.contains("custom.name === recordedGame.name")
            && js.contains("custom.legacy_ids.includes(recordedGame.id)")
            && js.contains("if (clip && customGameForRecordedGame(clip.game)) return null"),
        "historical collision aliases must retain custom icons without enabling plugin presentation"
    );
}

#[test]
fn resolved_recorder_media_root_updates_library_and_playback_scope() {
    let app = app_rs();
    let library = library_rs();

    assert!(
        app.contains("Event::MediaRootResolved")
            && app.contains("StorageSettings>()")
            && app.contains("set_media_dir")
            && library.contains("allow_local_clip_asset")
            && library.contains("allow_file")
            && app.contains("service::prepare_writable_media_directory(&media_dir)?"),
        "recorder fallback must publish the actual media root and Library results must exact-scope playback files"
    );
}

#[test]
fn renderer_filesystem_authority_is_exact_and_backend_owned() {
    let app = app_rs();
    let cloud = cloud_rs();
    let library = library_rs();
    let settings_js = read_ui_js("settings.js");
    let config_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tauri.conf.json");
    let config: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(config_path).unwrap()).unwrap();
    let static_scope = config["app"]["security"]["assetProtocol"]["scope"]
        .as_array()
        .expect("asset protocol scope array");

    assert!(
        static_scope.is_empty(),
        "asset scope must be granted per validated file"
    );
    assert!(!app.contains(".allow_directory("));
    assert!(!cloud.contains(".allow_directory("));
    assert!(library.contains(".allow_file("));
    assert!(cloud.contains(".allow_file("));
    assert!(app.contains("NativeMediaFolderAuthorization"));
    assert!(app.contains("validate_change(&old_media_dir, &media_dir)"));
    assert!(app.contains("fn extract_window_icon(process_id: u32)"));
    assert!(app.contains("crate::games::list_game_windows()"));
    assert!(settings_js.contains("processId: win.process_id"));
    assert!(!settings_js.contains("exePath: win.exe_path"));
}

#[test]
fn local_library_refresh_rejects_stale_snapshots_and_reports_event_errors() {
    let app_core = read_ui_js("app-core.js");
    let library = read_ui_js("library.js");
    let review = read_ui_js("review-player.js");
    let main = read_ui_js("main.js");
    let refresh_clips = js_function_body(&library, "refreshClips");

    assert!(
        app_core.contains("var localClipsRequestGate = CloudCore.createRequestGate();"),
        "local Library reads need a latest-request generation gate"
    );
    assert!(
        app_core.contains("var localLibraryWarning = \"\";"),
        "the UI must remember which visible error belongs to a partial Library scan"
    );
    for required in [
        "const request = localClipsRequestGate.begin(\"local-library\");",
        "isForegroundWorkCurrent(lifecycleWork)",
        "localClipsRequestGate.isCurrent(request, \"local-library\")",
        "if (!isCurrent()) return false;",
        "result = await invoke(\"list_clips\");",
        "freshClips = Array.isArray(result.clips) ? result.clips : [];",
        "applyLocalLibraryWarnings(result.warnings);",
        "clipsCache = freshClips;",
    ] {
        assert!(
            refresh_clips.contains(required),
            "refreshClips must arbitrate snapshots with `{required}`"
        );
    }
    assert!(
        refresh_clips.find("if (!isCurrent()) return false;")
            < refresh_clips.find("applyLocalLibraryWarnings(result.warnings);")
            && refresh_clips.find("applyLocalLibraryWarnings(result.warnings);")
                < refresh_clips.find("clipsCache = freshClips;"),
        "a stale response must be rejected before it mutates warnings or the local cache"
    );
    let apply_warnings = js_function_body(&library, "applyLocalLibraryWarnings");
    for required in [
        "error.textContent === localLibraryWarning",
        "error.textContent = \"\";",
        "localLibraryWarning = normalized.join(\" \");",
        "error.textContent = localLibraryWarning;",
    ] {
        assert!(
            apply_warnings.contains(required),
            "Library warnings must use safe, scoped notice handling: `{required}`"
        );
    }
    assert!(
        library.contains("function invalidateLocalClipsRefresh()")
            && js_function_body(&library, "replaceClipInCache")
                .contains("invalidateLocalClipsRefresh();")
            && js_function_body(&review, "applyDeletion")
                .contains("invalidateLocalClipsRefresh();")
            && review.matches("invalidateLocalClipsRefresh();").count() >= 2,
        "rename, delete, and export cache mutations must invalidate older snapshots"
    );
    assert!(
        main.contains("function requestRefresh()")
            && main.contains("refresh().catch((error) => {")
            && main.contains("listen(\"saved\"")
            && main.contains("listen(\"osu-enrichment-updated\"")
            && main.matches("requestRefresh();").count() >= 2,
        "fire-and-forget event refreshes must catch and surface current failures"
    );
}

#[test]
fn cloud_upload_completion_preserves_equivalent_paths_and_reports_local_deletion() {
    let library = read_ui_js("library.js");
    let cloud = read_ui_js("cloud.js");
    let refresh_clips = js_function_body(&library, "refreshClips");
    let upload = js_function_body(&cloud, "uploadClipToCloud");

    assert!(
        refresh_clips
            .contains("PlayerCore.sameClipPath(clip.path, currentPath)"),
        "post-upload Library refreshes must preserve reviews across equivalent Windows path spellings"
    );
    assert!(
        !refresh_clips.contains("clip.path === currentPath"),
        "active clip reconciliation must not use raw path-string equality"
    );
    assert!(
        refresh_clips.contains("currentClip = { ...fresh, path: currentPath };"),
        "Library reconciliation must preserve the active path spelling while merging fresh metadata"
    );

    let refresh = upload
        .find("const refreshCompleted = await refresh();")
        .expect("cloud upload completion refreshes the authoritative local Library");
    let feedback = upload
        .find("finishPostRefreshFeedback(refreshCompleted, {")
        .expect("cloud upload completion settles feedback against the refresh result");
    assert!(
        refresh < feedback
            && upload.contains("error: result && result.record ? result.record.error : \"\",")
            && upload.contains(
                "notice: result && result.local_deleted\n        ? \"cloud upload ready · local copy deleted\""
            ),
        "intentional deletion and upload errors must be delivered only after the authoritative refresh settles"
    );
}

#[test]
fn post_upload_feedback_waits_for_a_completed_foreground_refresh() {
    let app_core = read_ui_js("app-core.js");
    let refresh = js_function_body(&app_core, "refresh");
    let finish = js_function_body(&app_core, "finishPostRefreshFeedback");
    let flush = js_function_body(&app_core, "flushDeferredPostRefreshFeedback");

    assert!(
        app_core.contains("var pendingPostRefreshFeedback = null;"),
        "background upload feedback needs one bounded pending slot"
    );
    for required in [
        "if (refreshCompleted)",
        "showPostRefreshFeedback(feedback);",
        "pendingPostRefreshFeedback = {",
        "error: error || pending.error || \"\",",
        "notice: notice || pending.notice || \"\",",
    ] {
        assert!(
            finish.contains(required),
            "post-refresh feedback settlement must include `{required}`"
        );
    }
    assert!(
        refresh.contains("if (completed) flushDeferredPostRefreshFeedback();"),
        "a completed foreground refresh must publish feedback queued while backgrounded"
    );
    assert!(
        flush.contains("pendingPostRefreshFeedback = null;")
            && flush.contains("showPostRefreshFeedback(feedback);"),
        "deferred feedback must be consumed exactly once"
    );
}

#[test]
fn keyboard_shortcuts_document_j_l_frame_step_and_arrows_seek() {
    let html = index_html();

    assert!(
        html.contains("<div><dt><kbd>J</kbd> <kbd>L</kbd></dt><dd>Step 10 frames</dd></div>"),
        "shortcut help must document J/L as the frame-step controls"
    );
    assert!(
        html.contains("<div><dt><kbd>&larr;</kbd> <kbd>&rarr;</kbd></dt><dd>Back / forward 5s (<kbd>&#8679;</kbd> 1s)</dd></div>"),
        "shortcut help must document arrow keys as the coarse seek controls"
    );
}

#[test]
fn settings_opens_as_popup_and_guards_unsaved_discard() {
    let html = index_html();
    let js = main_js();
    let css = styles_css();

    for required in [
        "id=\"settings-page\" class=\"settings-page\" hidden role=\"dialog\" aria-modal=\"true\"",
        "id=\"settings-title\"",
        "id=\"settings-popup-shell\"",
        "id=\"settings-discard-warning\"",
        "Careful--your changes aren't saved.",
    ] {
        assert!(
            html.contains(required),
            "settings popup markup must include `{required}`"
        );
    }

    assert!(
        html.contains(
            "<button id=\"settings-close\" type=\"button\">Close</button>\n          <span id=\"settings-discard-warning\""
        ),
        "settings discard warning must render next to the footer close/discard button"
    );

    for required in [
        ".settings-popup-shell",
        ".settings-discard-warning",
        ".settings-save-glow",
        ".settings-shake",
        "@keyframes settings-shake",
        "@keyframes settings-save-glow",
    ] {
        assert!(
            css.contains(required),
            "settings popup CSS must include `{required}`"
        );
    }

    let popup_shell_rule = css_rule_body(&css, ".settings-popup-shell");
    assert!(
        css_decl_value(popup_shell_rule, "border-radius").is_some()
            && css_decl_value(popup_shell_rule, "overflow") == Some("hidden"),
        "settings popup shell must clip child backgrounds to preserve all rounded corners"
    );

    for required in [
        "function stableSettingsSnapshot(value)",
        "function settingsHaveUnsavedChanges()",
        "function syncSettingsDirtyState",
        "function showSettingsDiscardWarning()",
        "function resetSettingsDiscardWarning()",
        "function requestSettingsClose({ allowDiscard = true } = {})",
        "if (!settingsDiscardWarningArmed || !allowDiscard)",
        "$(\"settings-close\").textContent = dirty ? \"Discard Changes\" : \"Close\"",
        "$(\"settings-save\").classList.toggle(\"settings-save-glow\"",
        "$(\"settings-discard-warning\").textContent = \"Careful--your changes aren't saved.\"",
        "$(\"rail-settings\").addEventListener(\"click\", () => {",
        "$(\"settings-close\").addEventListener(\"click\", requestSettingsClose)",
        "$(\"settings-page\").addEventListener(\"pointerdown\", (ev) => {",
        "if (ev.target === $(\"settings-page\")) requestSettingsClose({ allowDiscard: false });",
        "requestSettingsClose();",
    ] {
        assert!(
            js.contains(required),
            "settings popup JS must include `{required}`"
        );
    }

    assert!(
        js.contains("$(\"review-viewer\").hidden = !currentClip")
            && js.contains("$(\"gallery-view\").hidden = !!currentClip"),
        "settings popup must not hide the underlying review/gallery view"
    );
}

#[test]
fn settings_marks_changed_rows_and_tabs() {
    let html = index_html();
    let js = main_js();
    let css = styles_css();

    for required in [
        "data-settings-key=\"open_on_startup\"",
        "data-settings-key=\"capture_mode capture_region window_title\"",
        "data-settings-key=\"audio.output_enabled audio.output_device_id audio.output_volume audio.split_output_by_process\"",
        "data-settings-key=\"games.plugins\"",
        "data-settings-key=\"games.custom_games\"",
        "data-settings-key=\"cloud.default_visibility\"",
        "data-settings-key=\"hotkey hotkey_secondary\"",
    ] {
        assert!(
            html.contains(required),
            "settings dirty indicator markup must include `{required}`"
        );
    }

    for required in [
        ".setting-changed",
        ".settings-tabs .tab.settings-tab-changed::after",
    ] {
        assert!(
            css.contains(required),
            "settings dirty indicator CSS must include `{required}`"
        );
    }

    for required in [
        "var settingsIndicatorBaseline = null;",
        "function settingsValueAtPath(source, path)",
        "function settingKeyChanged(path, draft, baseline)",
        "function syncSettingsChangeIndicators()",
        "node.classList.toggle(\"setting-changed\", changed)",
        "tab.classList.toggle(\"settings-tab-changed\", changed)",
        "settingsIndicatorBaseline = readSettings();",
        "row.dataset.settingsKey = `games.plugins.${plugin.id}`;",
        "row.dataset.settingsKey = `games.custom_games.${game.id}`;",
    ] {
        assert!(
            js.contains(required),
            "settings dirty indicator JS must include `{required}`"
        );
    }
}

#[test]
fn settings_popup_review_feedback_edges_are_guarded() {
    let js = main_js();

    for required in [
        "function settingsBaselineForComparison()",
        "function stripEphemeralSettingsState(value)",
        "delete cloud.uploads;",
        "function resetSettingsBaselineFromForm()",
        "function refreshSettingsBaselineIfClean()",
        "function syncSettingsDraftFromForm({ resetDiscard = true } = {})",
        "syncSettingsDraftFromForm({ resetDiscard: false });",
        "function syncSettingsModalBackground()",
        "document.querySelector(\".sidebar\")",
        "node.inert = settingsOpen;",
        "node.setAttribute(\"aria-hidden\", settingsOpen ? \"true\" : \"false\")",
        "$(\"settings-page\").addEventListener(\"pointerdown\", (ev) => {",
        "if (ev.target === $(\"settings-page\")) requestSettingsClose({ allowDiscard: false });",
        "if (settingsOpen) {",
        "showSettingsDiscardWarning();",
        "return;",
        "refreshSettingsBaselineIfClean();",
        "row.dataset.settingsKey = `games.custom_games.${game.id}`;",
    ] {
        assert!(
            js.contains(required),
            "settings popup review feedback JS must include `{required}`"
        );
    }

    assert!(
        !js.contains("$(\"settings-page\").addEventListener(\"click\", (ev) => {\n  if (ev.target === $(\"settings-page\")) requestSettingsClose({ allowDiscard: false });\n});"),
        "settings backdrop close guard must not use click because drag release can dispatch click on the overlay"
    );
}

#[test]
fn osu_play_blocks_are_centered_and_taller_in_timeline() {
    let css = styles_css();
    let timeline_rule = css_rule_body(&css, ".timeline-main");
    let layer_rule = css_rule_body(&css, ".play-block-layer");
    let block_rule = css_rule_body(&css, ".play-block {");
    let incomplete_rule = css_rule_body(&css, ".play-block.incomplete");
    let active_rule = css_rule_body(&css, ".play-block.active,");

    assert_eq!(
        css_decl_value(timeline_rule, "height"),
        Some("56px"),
        "timeline band height anchors the centered osu! play block placement"
    );
    assert_eq!(
        css_decl_value(layer_rule, "top"),
        Some("18px"),
        "osu! play blocks should sit vertically centered in the timeline band"
    );
    assert_eq!(
        css_decl_value(layer_rule, "height"),
        Some("20px"),
        "osu! play block hit area should stay taller than the old compact rail"
    );
    assert_eq!(
        css_decl_value(block_rule, "height"),
        Some("20px"),
        "osu! play block visuals should fill the taller hit area"
    );
    assert!(
        main_js().contains("+ (play.incomplete ? \" incomplete\" : \"\")")
            && css_decl_value(incomplete_rule, "border-color").is_some()
            && css_decl_value(incomplete_rule, "background").is_some(),
        "incomplete osu! play blocks should receive their own purple timeline styling"
    );
    assert_eq!(
        css_decl_value(active_rule, "z-index"),
        Some("8"),
        "active osu! play blocks should paint above overlapping neighbors"
    );
}

#[test]
fn osu_play_rail_click_holds_selected_play_during_seek() {
    let js = main_js();

    assert!(
        js.contains("var selectedGamePlayIndex = -1")
            && js.contains("var selectedGamePlayPending = false")
            && js.contains("function selectGamePlay(index, playStart, playEnd)")
            && js.contains("selectedGamePlayPending = true;")
            && js.contains("if (options.keepGamePlaySelection || selectedGamePlayPending)")
            && js.contains("if (inSelectedPlay) selectedGamePlayPending = false;")
            && js.contains("selectGamePlay(index, play.start, play.end);")
            && js.contains("seekTo(play.start, { keepGamePlaySelection: true });")
            && js.contains("if (!options.keepGamePlaySelection) clearGamePlaySelection();")
            && js.contains("syncGamePlayRail(target, { keepGamePlaySelection: options.keepGamePlaySelection });")
            && js.contains("playActiveIndex(clipPlays(), currentTime, selectedIndex)"),
        "Set plays clicks should highlight the clicked play immediately instead of waiting for the video seek to settle"
    );
}

#[test]
fn osu_account_settings_use_direct_api_credentials_and_guide() {
    let js = main_js();
    let app = app_rs();

    assert!(
        js.contains("invoke(\"save_osu_api_settings\"")
            && js.contains("invoke(\"test_osu_api_connection\"")
            && js.contains("invoke(\"open_osu_api_setup_guide\"")
            && app.contains("crate::osu_api::save_osu_api_settings")
            && app.contains("crate::osu_api::test_osu_api_connection")
            && app.contains("crate::osu_api::open_osu_api_setup_guide"),
        "osu! account settings must call direct osu! API commands instead of Cloud proxy commands"
    );
    assert!(
        js.contains("Client ID")
            && js.contains("Client Secret")
            && js.contains("osu! User ID or Username")
            && js.contains("Test osu! API connection")
            && js.contains("setAttribute(\"aria-label\", \"Open osu! API setup guide\")"),
        "osu! account settings should collect direct API credentials and expose a setup guide button"
    );
    assert!(
        !js.contains("Connect Clipline Cloud to enable osu! login.")
            && !js.contains("Login with osu!")
            && !js.contains("cloud_osu_login")
            && !js.contains("cloud_osu_connection")
            && !app.contains("crate::cloud::cloud_osu_login")
            && !app.contains("crate::cloud::cloud_osu_connection"),
        "the old Cloud osu! login path should not stay user-visible once direct API credentials are used"
    );
}

#[test]
fn osu_play_rail_uses_thumbnail_metadata_rows() {
    let js = main_js();
    let core = player_core_js();
    let html = index_html();
    let css = styles_css();

    for required in [
        "game-play-thumb",
        "game-play-body",
        "game-play-song",
        "game-play-difficulty",
        "game-play-mods",
        "game-play-stars",
    ] {
        assert!(
            js.contains(required) && css.contains(required),
            "osu! play rail must render and style `{required}`"
        );
    }

    assert!(
        core.contains("coverUrl")
            && core.contains("starRating")
            && core.contains("\"CL\"")
            && core.contains("\"NOMOD\"")
            && core.contains("Incomplete")
            && core.contains("playExportRange")
            && !core.contains("\"estimated start\""),
        "osu! play rail formatting should expose thumbnails/stars, hide CL/nomod, mark incomplete plays, and avoid estimated-start copy"
    );
    assert!(
        html.contains("id=\"clip-menu-export-play\"")
            && js.contains("function showGamePlayContextMenu")
            && js.contains("function exportPlayClip")
            && js.contains("gamePlayContextTarget")
            && js.contains("clip-menu-export-play")
            && js.contains("includeMarkers: false")
            && js.contains("title: target.title"),
        "Set plays rows must use the app-owned context menu to export a play as a clean titled clip"
    );
}

#[test]
fn library_refresh_starts_osu_enrichment_retry() {
    let library = library_rs();

    assert!(
        library.contains("pub async fn list_clips<R: Runtime>")
            && library.contains("app: AppHandle<R>")
            && library.contains(
                "crate::osu_api::retry_pending_enrichment(&enrichment_app, retry_root).await",
            ),
        "list_clips should kick off the async osu! retry path during library refresh"
    );
}

#[test]
fn library_refresh_canonicalizes_the_media_root_once_before_scoping_clips() {
    let library = library_rs();
    let list_start = library
        .find("pub async fn list_clips<R: Runtime>")
        .expect("list_clips command");
    let list_end = library[list_start..]
        .find("\nfn list_clips_from_dir")
        .map(|offset| list_start + offset)
        .expect("list_clips helper follows command");
    let list = &library[list_start..list_end];

    assert!(list.contains("let canonical_scope_root = canonical_media_root(&scope_root)?;"));
    assert!(list.contains("allow_local_clip_asset_from_canonical_root("));
    assert_eq!(
        list.matches("canonical_media_root(&scope_root)").count(),
        1,
        "the unchanging media root should be resolved once per Library refresh"
    );
}

#[test]
fn game_event_rail_does_not_run_on_every_animation_frame() {
    let js = main_js();
    let schedule_overlay = js_function_body(&js, "scheduleOverlayIdleCheck");

    assert!(
        !js.contains("function animatePlayhead")
            && !js.contains("requestAnimationFrame(animatePlayhead)")
            && !js.contains("cancelAnimationFrame(rafId)")
            && !js.contains("let rafId"),
        "playback should not keep vestigial requestAnimationFrame bookkeeping after rail sync moved to media events"
    );
    assert!(
        js.contains("gameEventRows = []")
            && js.contains("gameEventRows.push(button)")
            && !js.contains("document.querySelectorAll(\"[data-game-event-index]\")"),
        "event rail active-state updates should use cached row elements instead of querying the DOM each tick"
    );
    assert!(
        js.contains("video.addEventListener(\"timeupdate\"")
            && js.contains("const current = reviewPlayheadTime();")
            && js.contains("syncGameEventRail(current);"),
        "timeupdate should keep the event rail following playback without tying it to requestAnimationFrame"
    );
    assert!(
        schedule_overlay.contains("clearOverlayIdleCheck();")
            && schedule_overlay.contains("updateOverlay();")
            && schedule_overlay.contains("setTimeout")
            && schedule_overlay.contains("overlayTimerId = 0;")
            && schedule_overlay.contains("OVERLAY_HIDE_MS"),
        "overlay idle fade should use a one-shot timer instead of a playback-frame polling loop"
    );
}

#[test]
fn rail_shows_save_hotkey() {
    let html = index_html();
    let js = main_js();
    let css = styles_css();

    assert!(
        html.contains("id=\"rail-hotkey\""),
        "sidebar rail must expose the current save hotkey"
    );
    assert!(
        js.contains("function updateHotkeyLabels(")
            && js.contains("rail-hotkey")
            && js.contains("Save Replay ("),
        "main.js must keep rail and button hotkey labels in sync"
    );
    assert!(
        css.contains(".rail-hotkey"),
        "rail hotkey needs stable compact styling"
    );
}

#[test]
fn rail_shows_connected_cloud_identity() {
    let html = index_html();
    let js = main_js();
    let css = styles_css();

    for required in [
        "<button id=\"rail-profile\"",
        "id=\"rail-profile-avatar\"",
        "id=\"rail-profile-name\"",
    ] {
        assert!(
            html.contains(required),
            "rail profile markup must include `{required}`"
        );
    }
    for required in [
        "function syncRailProfile",
        "function refreshRailProfileIdentity",
        "function loadRailProfileAvatar",
        "function openRailProfile",
        "invoke(\"cloud_user_profile\")",
        "invoke(\"cloud_user_avatar\")",
        "invoke(\"open_cloud_user_profile\")",
        "connected_display_name",
        "railProfileAvatarKey",
        "$(\"rail-profile-name\")",
    ] {
        assert!(
            js.contains(required),
            "main.js must wire cloud rail profile behavior through `{required}`"
        );
    }
    for required in [
        ".rail-profile",
        ".rail-profile[hidden]",
        ".rail-profile-avatar",
        ".rail-profile-name",
    ] {
        assert!(
            css.contains(required),
            "rail cloud identity needs stable compact styling for `{required}`"
        );
    }
    assert!(
        app_rs().contains("crate::cloud::cloud_user_avatar"),
        "native command registry must expose cloud_user_avatar for the rail profile"
    );
    assert!(
        app_rs().contains("crate::cloud::cloud_user_profile"),
        "native command registry must expose cloud_user_profile for display-name refresh"
    );
    assert!(
        app_rs().contains("crate::cloud::open_cloud_user_profile"),
        "native command registry must expose open_cloud_user_profile for the rail profile button"
    );
}

#[test]
fn opening_multitrack_clip_starts_direct_and_prepares_default_sidecars() {
    let app_core = read_ui_js("app-core.js");
    let reset_selection = js_function_body(&app_core, "resetSelectedAudioTracks");
    let review = read_ui_js("review-player.js");
    let open_clip = js_function_body(&review, "openClip");

    assert!(reset_selection.contains("defaultAudioTrackIds(clip)"));
    assert!(!reset_selection.contains("directPlaybackAudioTrackIds"));
    assert!(open_clip.contains("resetSelectedAudioTracks(clip);"));
    assert!(open_clip.contains(
        "currentReviewAudioTrackIds = PlayerCore.directPlaybackAudioTrackIds(clipAudioTracks(clip));"
    ));
    assert!(open_clip.contains("assignReviewVideoSource(clip.path, { resumeTime: 0 })"));
    assert!(open_clip.contains("video.play().catch(() => syncPlayState());"));
    assert!(open_clip.contains("requestSelectedAudioPreview();"));
    assert!(
        open_clip.find("video.play().catch(() => syncPlayState());")
            < open_clip.find("requestSelectedAudioPreview();"),
        "direct playback should start before the selected sidecars are prepared"
    );
    assert!(!open_clip.contains("applySelectedAudioTracksToPlayback"));
    assert!(!main_js().contains("function applyDefaultAudioSelectionIfNeeded"));
}

#[test]
fn review_and_upload_audio_controls_render_exact_selected_ids() {
    let app_core = read_ui_js("app-core.js");
    let review_panel = js_function_body(&app_core, "renderAudioTrackPanel");
    let upload_panel = js_function_body(&app_core, "renderUploadAudioTracks");
    assert!(review_panel.contains("PlayerCore.reviewAudioTrackRowState"));
    assert!(review_panel.contains("PlayerCore.applyReviewAudioTrackToggle"));
    assert!(upload_panel.contains("PlayerCore.reviewAudioTrackRowState"));
    assert!(upload_panel.contains("PlayerCore.applyReviewAudioTrackToggle"));
}

#[test]
fn review_audio_pruning_preserves_fallback_and_muted_selection() {
    let app_core = read_ui_js("app-core.js");
    let prune = js_function_body(&app_core, "pruneSelectedAudioTracks");
    assert!(prune.contains("PlayerCore.selectedReviewAudioTrackIds"));
    assert!(!prune.contains("defaultAudioTrackIds"));
}

#[test]
fn review_player_applies_logical_seek_only_for_current_metadata() {
    let review = read_ui_js("review-player.js");
    let assign = js_function_body(&review, "assignReviewVideoSource");
    let clear_error_handler = js_function_body(&review, "clearReviewSourceErrorHandler");
    let release = js_function_body(&review, "releaseReviewVideoSource");
    assert!(assign.contains("PlayerCore.beginSourceAssignment("));
    assert!(assign.contains("PlayerCore.metadataSeekDecision("));
    assert!(assign.contains("assignment.sourceGeneration !== reviewSourceGeneration"));
    assert!(assign.contains("clearReviewSourceErrorHandler();"));
    assert!(
        assign.contains("reviewSourceErrorHandler = () => reportReviewSourceError(assignment);")
    );
    assert!(assign.contains("video.addEventListener(\"error\", reviewSourceErrorHandler);"));
    assert!(!assign.contains("video.addEventListener(\"error\", () => reportReviewSourceError(assignment), { once: true })"));
    assert!(clear_error_handler
        .contains("video.removeEventListener(\"error\", reviewSourceErrorHandler);"));
    assert!(release.contains("clearReviewSourceErrorHandler();"));

    let seek_to = js_function_body(&review, "seekTo");
    assert!(seek_to.contains("PlayerCore.requestLogicalSeek("));
    assert!(seek_to.contains("reviewSeekState.metadataGeneration === reviewSourceGeneration"));

    assert!(review.contains("PlayerCore.seekedDecision("));
    assert!(review.contains("function reportReviewSourceError(assignment)"));
    assert!(assign.contains("video.addEventListener(\"error\""));
    assert!(review.contains("function reviewPlayheadTime()"));
    let prohibited_legacy_identifier = ["pending", "Seek"].concat();
    let player_core = read_ui_js("player-core.js");
    let main = read_ui_js("main.js");
    let task_two_scope = [
        ("tests/player_core.rs", include_str!("player_core.rs")),
        ("tests/ui_contract.rs", include_str!("ui_contract.rs")),
        ("ui/player-core.js", player_core.as_str()),
        ("ui/review-player.js", review.as_str()),
        ("ui/main.js", main.as_str()),
    ];
    let legacy_identifier_files: Vec<_> = task_two_scope
        .iter()
        .filter_map(|(path, source)| {
            source
                .contains(&prohibited_legacy_identifier)
                .then_some(*path)
        })
        .collect();
    assert!(
        legacy_identifier_files.is_empty(),
        "Task 2 scope must not retain `{prohibited_legacy_identifier}`; found in {}",
        legacy_identifier_files.join(", "),
    );
    assert!(!review.contains("reviewSeekRevision"));
}

#[test]
fn audio_sidecar_preparation_consumes_validated_hits_once() {
    let library = library_rs();
    assert!(
        !library.contains("ordered_hits"),
        "validated cache hits must be retained in the ordered result instead of rebuilt"
    );
}

#[test]
fn explicit_audio_preview_uses_one_pure_coalescing_queue() {
    let review = read_ui_js("review-player.js");
    assert!(review.contains("var audioPreviewQueue = PlayerCore.emptyAudioPreviewQueue();"));
    assert!(review.contains("PlayerCore.queueAudioPreviewRequest("));
    assert!(review.contains("PlayerCore.finishAudioPreviewRequest("));
    assert_eq!(
        review
            .matches("await invoke(\"prepare_clip_audio_sidecars\"")
            .count(),
        1
    );
    assert!(!review.contains("invoke(\"preview_clip_audio_tracks\""));
    assert!(review.contains("protectedPreviewPaths"));
    assert!(review.contains("activeReviewAudioSidecars.map((sidecar) => sidecar.path)"));
    assert!(!review.contains("audioPreviewSeq"));
}

#[test]
fn audio_sidecar_transport_prepares_and_releases_hidden_media() {
    let app_core = read_ui_js("app-core.js");
    let review = read_ui_js("review-player.js");
    for state in [
        "var reviewAudioMode = \"direct\";",
        "var reviewAudioMuted = false;",
        "var reviewAudioVolume = 1;",
        "var activeReviewAudioSidecars = [];",
        "var reviewAudioSidecarGeneration = 0;",
        "var reviewAudioDriftTimer = 0;",
    ] {
        assert!(
            app_core.contains(state),
            "missing sidecar transport state `{state}`"
        );
    }

    let prepare = js_function_body(&review, "prepareReviewAudioSidecars");
    assert!(prepare.contains("new Audio()"));
    assert!(prepare.contains("audio.preload = \"auto\";"));
    assert!(prepare.contains("audio.muted = true;"));
    assert!(prepare.contains("audio.src = convertFileSrc(sidecar.path);"));
    assert!(prepare.contains("audio.addEventListener(\"canplay\""));
    assert!(prepare.contains("audio.addEventListener(\"error\""));

    let dispose = js_function_body(&review, "disposeReviewAudioSidecarSet");
    assert!(dispose.contains("audio.pause();"));
    assert!(dispose.contains("audio.removeAttribute(\"src\");"));
    assert!(dispose.contains("audio.load();"));
    let clear = js_function_body(&review, "clearReviewAudioSidecars");
    assert!(clear.contains("reviewAudioSidecarGeneration += 1;"));
    assert!(clear.contains("activeReviewAudioSidecars = [];"));
    assert!(clear.contains("clearReviewAudioDriftTimer();"));
}

#[test]
fn audio_sidecar_transport_follows_only_the_video_clock() {
    let review = read_ui_js("review-player.js");
    let main = read_ui_js("main.js");
    let sync = js_function_body(&review, "syncReviewAudioSidecarSet");
    assert!(sync.contains("PlayerCore.audioSidecarSyncDecision("));
    assert!(sync.contains("duration: audio.duration"));
    assert!(sync.contains("ended: audio.ended"));
    assert!(sync.contains("audio.currentTime = decision.seekTime;"));
    assert!(sync.contains("audio.playbackRate = decision.playbackRate;"));
    assert!(!sync.contains("video.currentTime ="));

    for event in ["play", "pause", "timeupdate", "ratechange"] {
        assert!(
            main.contains(&format!("video.addEventListener(\"{event}\"")),
            "video {event} must synchronize sidecars"
        );
    }
    assert!(main.contains("syncReviewAudioSidecars();"));
    let seeked = review
        .split("video.addEventListener(\"seeked\"")
        .nth(1)
        .and_then(|tail| tail.split("function seekBy").next())
        .expect("seeked handler");
    // Deliberately changed: this previously pinned `forceSeek: true`, i.e. *every*
    // video `seeked` bypassing the drift tolerance. Traced on a warm cache, that
    // made the initial source settlement re-seek already-audible sidecars backward
    // ~20 ms for no correction — far inside the tolerance, landing back where it
    // started — which was the audible repeat at the start of every clip. Forcing is
    // now driven by seek provenance, so a user reposition still realigns
    // immediately while settlement does not.
    assert!(seeked.contains("PlayerCore.sidecarRealignmentForced(decision.confirmedSource)"));
    assert!(
        !seeked.contains("forceSeek: true"),
        "settlement must not bypass the sidecar drift tolerance"
    );
    assert!(review.contains("window.setInterval(() => syncReviewAudioSidecars(), 500)"));
}

#[test]
fn audio_sidecar_transport_owns_logical_mute_volume_and_lifecycle() {
    let review = read_ui_js("review-player.js");
    let main = read_ui_js("main.js");
    let output = js_function_body(&review, "applyReviewAudioOutput");
    assert!(output.contains("PlayerCore.reviewAudioOutputDecision("));
    assert!(output.contains("video.muted = decision.videoMuted;"));
    assert!(output.contains("audio.muted = decision.sidecarMuted;"));

    let sync_volume = js_function_body(&review, "syncVolume");
    assert!(sync_volume.contains("reviewAudioMuted"));
    assert!(sync_volume.contains("reviewAudioVolume"));
    let toggle_mute = js_function_body(&review, "toggleMute");
    assert!(toggle_mute.contains("reviewAudioMuted"));
    assert!(!toggle_mute.contains("video.muted"));
    assert!(main.contains("reviewAudioVolume = Number($(\"volume-slider\").value);"));
    assert!(main.contains("applyReviewAudioOutput();"));

    for lifecycle in [
        "assignReviewVideoSource",
        "releaseReviewVideoSource",
        "releaseVideoFileHandle",
        "suspendReviewPlayback",
        "openClip",
        "closeReview",
    ] {
        assert!(
            js_function_body(&review, lifecycle).contains("clearReviewAudioSidecars("),
            "{lifecycle} must clear sidecar file handles and callbacks"
        );
    }
}

#[test]
fn preview_failure_keeps_source_and_reverts_controls_to_audible_selection() {
    let review = read_ui_js("review-player.js");
    let restore = js_function_body(&review, "restoreAudibleAudioSelection");
    assert!(restore.contains("selectedAudioTrackIds = new Set(currentReviewAudioTrackIds);"));
    assert!(restore.contains("renderAudioTrackPanel();"));
    assert!(restore.contains("setDeckStatus(message, { transient: true });"));
    assert!(!restore.contains("setReviewVideoSource"));
}

#[test]
fn valid_sidecar_activation_reads_latest_player_state_without_swapping_video() {
    let review = read_ui_js("review-player.js");
    let run = js_function_body(&review, "runAudioPreviewRequest");
    let await_preview = run
        .find("await invoke(\"prepare_clip_audio_sidecars\"")
        .unwrap();
    let prepare = run[await_preview..]
        .find("await prepareReviewAudioSidecars(")
        .unwrap();
    assert!(await_preview < prepare);
    assert!(!run.contains("setReviewVideoSource"));
    assert!(!run.contains("assignReviewVideoSource"));
    assert!(!run.contains("video.src"));

    let activate = js_function_body(&review, "activatePreparedReviewAudioSidecars");
    assert!(activate.contains("currentTime: reviewPlayheadTime()"));
    assert!(activate.contains("playbackRate: video.playbackRate"));
    assert!(activate.contains("paused: video.paused"));
    assert!(activate.contains("ended: video.ended"));
    let await_play = activate
        .find("await syncReviewAudioSidecarSet(")
        .expect("activation waits for every muted sidecar play promise");
    let install = activate
        .find("activeReviewAudioSidecars = prepared;")
        .expect("complete prepared set is installed atomically");
    let switch_output = activate
        .find("reviewAudioMode = \"sidecars\";")
        .expect("sidecar output becomes audible only after readiness/play succeeds");
    assert!(await_play < install && install < switch_output);
    assert!(activate[install..].contains("applyReviewAudioOutput();"));
}

#[test]
fn audio_sidecar_activation_is_generation_gated_and_disposes_stale_sets() {
    let review = read_ui_js("review-player.js");
    let run = js_function_body(&review, "runAudioPreviewRequest");
    assert!(run.contains("previewRequestStillCurrent(request)"));
    assert!(run.contains("PlayerCore.finishAudioPreviewRequest("));
    assert!(run.contains("transition.apply"));
    assert!(run.contains("disposeReviewAudioSidecarSet(prepared);"));
    assert!(run.contains("if (transition.start) void runAudioPreviewRequest(transition.start);"));

    let current = js_function_body(&review, "previewRequestStillCurrent");
    assert!(current.contains("request.sourceGeneration === reviewSourceGeneration"));
    assert!(current.contains("request.sidecarGeneration === reviewAudioSidecarGeneration"));
    let activate = js_function_body(&review, "activatePreparedReviewAudioSidecars");
    assert!(
        activate
            .matches("previewRequestStillCurrent(request)")
            .count()
            >= 2
    );
}

#[test]
fn direct_and_muted_audio_selections_clear_sidecars_without_changing_video_source() {
    let review = read_ui_js("review-player.js");
    let request = js_function_body(&review, "requestSelectedAudioPreview");
    assert!(request.contains("if (selected.length === 0)"));
    assert!(request.contains("clearReviewAudioSidecars(\"muted\");"));
    assert!(request.contains("clearReviewAudioSidecars(\"direct\");"));
    assert!(!request.contains("setReviewVideoSource"));
    assert!(!request.contains("assignReviewVideoSource"));
    assert!(!request.contains("video.src"));
}

#[test]
fn returning_to_fallback_invalidates_an_inflight_audio_preview() {
    let review = read_ui_js("review-player.js");
    let request = js_function_body(&review, "requestSelectedAudioPreview");
    let needs_preview = request
        .find("if (!PlayerCore.reviewSelectionNeedsPreview(tracks, selected)) {")
        .expect("fallback selection is gated on reviewSelectionNeedsPreview");
    let cancel = request[needs_preview..]
        .find("cancelDesiredAudioPreview();")
        .map(|offset| needs_preview + offset)
        .expect("returning to fallback playback must cancel queued preview work");
    assert!(
        needs_preview < cancel,
        "a fallback selection must cancel an in-flight/queued preview before falling back to direct playback"
    );
}

#[test]
fn timeline_and_media_events_render_the_logical_playhead() {
    let review = read_ui_js("review-player.js");
    let main = read_ui_js("main.js");
    assert!(js_function_body(&review, "paintTimeline").contains("reviewPlayheadTime()"));
    assert!(js_function_body(&review, "paintOverview").contains("reviewPlayheadTime()"));
    assert!(js_function_body(&review, "seekBy").contains("reviewSeekState.targetTime"));
    assert!(main.contains("const current = reviewPlayheadTime();"));
}

#[test]
fn opening_a_clip_clears_only_the_previous_clips_seek_state() {
    let review = read_ui_js("review-player.js");
    let open_clip = js_function_body(&review, "openClip");
    assert!(open_clip.contains("reviewSeekState = PlayerCore.createLogicalSeekState();"));
    assert!(open_clip.contains("assignReviewVideoSource(clip.path, { resumeTime: 0 })"));
}

#[test]
fn every_review_video_source_mutation_uses_generation_helpers() {
    let review = read_ui_js("review-player.js");
    assert_eq!(
        review.matches("video.src = convertFileSrc(path);").count(),
        1
    );
    assert_eq!(review.matches("video.removeAttribute(\"src\");").count(), 1);
    assert_eq!(review.matches("video.load();").count(), 1);

    let restore_rename = js_function_body(&review, "restoreVideoAfterRename");
    assert!(restore_rename.contains("setReviewVideoSource(path, {"));
    let set_source = js_function_body(&review, "setReviewVideoSource");
    assert!(set_source
        .contains("assignReviewVideoSource(path, { resumeTime, onLoadedMetadata: restore })"));
    let open_clip = js_function_body(&review, "openClip");
    assert!(open_clip.contains("assignReviewVideoSource(clip.path, { resumeTime: 0 })"));

    for name in [
        "releaseVideoFileHandle",
        "suspendReviewPlayback",
        "closeReview",
    ] {
        assert!(
            js_function_body(&review, name).contains("releaseReviewVideoSource();"),
            "{name} must invalidate source ownership before releasing video.src"
        );
    }
}

#[test]
fn native_background_lifecycle_releases_heavy_frontend_state() {
    let app = app_rs();
    let js = main_js();
    let tray_start = app
        .find("fn send_main_window_to_tray")
        .expect("send-to-tray helper");
    let tray_end = app[tray_start..]
        .find("fn quit_app")
        .map(|offset| tray_start + offset)
        .expect("quit helper follows tray helper");
    let tray_helper = &app[tray_start..tray_end];
    let suspend_start = js
        .find("function suspendReviewPlayback(")
        .expect("frontend suspend helper");
    let close_review_start = js.find("function closeReview()").unwrap();
    let suspend_helper = &js[suspend_start..close_review_start];

    assert!(
        tray_helper.contains("WindowLifecycleMode::Destroying")
            && tray_helper.contains("window.destroy()")
            && tray_helper.contains("WINDOW_LIFECYCLE_EVENT"),
        "native close-to-tray must destroy the webview and publish Destroying"
    );
    assert!(
        !app.contains("fn hide_autostart_webviews"),
        "create:false makes hide_autostart_webviews obsolete"
    );
    assert!(
        js.contains("listen(\"window-lifecycle\"")
            && js.contains("applyWindowLifecycleSnapshot(event.payload)"),
        "frontend must consume revisioned native lifecycle snapshots"
    );
    let ready = js_function_body(&js, "reportFrontendReady");
    let listener_ready = ready
        .find("await windowLifecycleListenerReady")
        .expect("frontend_ready waits for lifecycle listener registration");
    let ready_invoke = ready
        .find("invoke(\"frontend_ready\")")
        .expect("frontend_ready invoke");
    assert!(
        listener_ready < ready_invoke,
        "the durable snapshot must be requested only after lifecycle events can be received"
    );
    assert!(
        suspend_helper.contains("cancelDesiredAudioPreview();")
            && suspend_helper.contains("clearOverlayIdleCheck();")
            && suspend_helper.contains("video.pause();")
            && suspend_helper.contains("releaseReviewVideoSource();"),
        "suspending playback must cancel preview work, stop overlay timers, and unload the video"
    );
    assert!(
        suspend_helper.contains("currentClip = null;")
            && suspend_helper.contains("currentReviewMediaPath = null;")
            && suspend_helper.contains("updateViews();"),
        "suspending playback must also leave the editor state so reopening from tray cannot show a src-less current clip"
    );
    let release = js_function_body(&js, "releaseBackgroundUi");
    for required in [
        "localClipsRequestGate.invalidate();",
        "cloudClipsRequestGate.invalidate();",
        "releaseBackgroundSettingsUi();",
        "suspendReviewPlayback({ renderGallery: false });",
        "clearHeavyGalleryDom();",
    ] {
        assert!(
            release.contains(required),
            "background entry must release/invalidate `{required}`"
        );
    }
    assert!(
        !release.contains("renderClips()"),
        "background teardown must not immediately rebuild gallery DOM"
    );
}

#[test]
fn lifecycle_guards_refreshes_posters_and_cloud_media_completions() {
    let app_core = read_ui_js("app-core.js");
    let library = read_ui_js("library.js");
    let cloud = read_ui_js("cloud.js");
    let main = read_ui_js("main.js");
    let settings = read_ui_js("settings.js");
    let review = read_ui_js("review-player.js");

    assert!(
        app_core.contains("WindowLifecycleCore.requestRefresh(windowLifecycleState)")
            && main.contains("if (!requestWindowRefresh()) return;"),
        "background event bursts must coalesce into a single dirty refresh"
    );
    let refresh_clips = js_function_body(&library, "refreshClips");
    assert!(
        refresh_clips.contains("isForegroundWorkCurrent(lifecycleWork)")
            && refresh_clips.contains("localClipsRequestGate.isCurrent("),
        "local library responses must satisfy both lifecycle and request generations"
    );
    let poster = js_function_body(&library, "loadCardPoster");
    assert!(
        poster.contains("captureForegroundWork()")
            && poster.contains("isForegroundWorkCurrent(lifecycleWork)"),
        "poster completions from an old foreground generation must be ignored"
    );
    let cloud_open = js_function_body(&cloud, "openCloudEntryInApp");
    let stale_guard = cloud_open
        .find("if (!isForegroundWorkCurrent(lifecycleWork)) return;")
        .expect("cloud download stale-generation guard");
    let open_clip = cloud_open
        .find("openClip({")
        .expect("cloud download opens a clip");
    assert!(
        stale_guard < open_clip,
        "a cloud download completing after background entry must not recreate media"
    );
    assert!(
        main.contains("transition.missedBackground") && main.contains("releaseBackgroundUi();"),
        "a revision gap returning to foreground must reconcile missed teardown"
    );
    let mic_test = js_function_body(&settings, "testMic");
    assert!(
        mic_test.contains("captureForegroundWork()")
            && mic_test.contains("isForegroundWorkCurrent(lifecycleWork)"),
        "microphone setup must not complete into an old foreground generation"
    );
    assert!(
        review.contains("function reviewAsyncWorkStillCurrent(")
            && review.contains("reviewSourceGeneration === expectedSourceGeneration"),
        "rename completion must require both lifecycle and review-source generations"
    );
    let initial_settings = js_function_body(&main, "loadInitialSettings");
    assert!(
        initial_settings.contains("isForegroundWorkCurrent(lifecycleWork)")
            && main.contains("foregroundBootPromise")
            && main.contains("foregroundBootCompleted"),
        "initial settings work must be generation guarded and retryable after failure"
    );
}

#[test]
fn initial_settings_tab_state_matches_visible_section() {
    let html = index_html();
    let tabs_start = html.find("id=\"settings-tabs\"").expect("settings tabs");
    let tabs_end = html[tabs_start..]
        .find("</nav>")
        .map(|offset| tabs_start + offset)
        .expect("settings tabs close");
    let tabs = &html[tabs_start..tabs_end];

    let mut active_tabs = Vec::new();
    let mut cursor = 0;
    while let Some(offset) = tabs[cursor..].find("<button") {
        let start = cursor + offset;
        let end = tabs[start..]
            .find('>')
            .map(|tag_end| start + tag_end)
            .expect("tab button closes");
        let tag = &tabs[start..=end];
        if tag_attr(tag, "class")
            .is_some_and(|class| class.split_whitespace().any(|c| c == "active"))
        {
            active_tabs.push(tag_attr(tag, "data-tab").expect("active tab has data-tab"));
        }
        cursor = end + 1;
    }
    assert_eq!(
        active_tabs.len(),
        1,
        "settings must have exactly one active initial tab"
    );
    let active_tab = active_tabs[0];

    let mut visible_sections = Vec::new();
    cursor = 0;
    while let Some(offset) = html[cursor..].find("<div class=\"settings-section\"") {
        let start = cursor + offset;
        let end = html[start..]
            .find('>')
            .map(|tag_end| start + tag_end)
            .expect("settings section opens");
        let tag = &html[start..=end];
        let section = tag_attr(tag, "data-section").expect("settings section has data-section");
        let hidden = tag
            .split_whitespace()
            .any(|part| part == "hidden" || part == "hidden>");
        if hidden {
            assert_ne!(
                section, active_tab,
                "the initially active settings section must not be hidden"
            );
        } else {
            visible_sections.push(section);
        }
        cursor = end + 1;
    }
    assert_eq!(
        visible_sections,
        vec![active_tab],
        "only the active settings tab's section should be visible before first interaction"
    );
}

#[test]
fn settings_tabs_preserve_unsaved_draft_until_save() {
    let js = main_js();
    let tab_handler_start = js
        .find("document.querySelectorAll(\"#settings-tabs .tab\")")
        .expect("settings tab handler");
    let timeline_start = js[tab_handler_start..]
        .find("$(\"timeline\")")
        .map(|offset| tab_handler_start + offset)
        .expect("timeline handler follows settings tabs");
    let tab_handler = &js[tab_handler_start..timeline_start];
    let save_handler_start = js
        .find("$(\"settings-save\").addEventListener")
        .expect("settings save handler");
    let video_start = js[save_handler_start..]
        .find("video.addEventListener")
        .map(|offset| save_handler_start + offset)
        .expect("video handlers follow settings save");
    let save_handler = &js[save_handler_start..video_start];
    let sync_start = js
        .find("function syncSettingsDraftFromForm({ resetDiscard = true } = {})")
        .expect("settings draft sync helper");
    let fill_start = js[sync_start..]
        .find("function fillSettings")
        .map(|offset| sync_start + offset)
        .expect("fillSettings follows settings draft sync helper");
    let sync_helper = &js[sync_start..fill_start];

    assert!(
        js.contains("settingsDraft = null")
            && js.contains("function settingsFormSource()")
            && js.contains("function syncSettingsDraftFromForm({ resetDiscard = true } = {})"),
        "settings must keep an explicit unsaved draft while the settings page is open"
    );
    assert!(
        tab_handler.contains("syncSettingsDraftFromForm();"),
        "switching tabs must snapshot edits before the current section is hidden"
    );
    assert!(
        save_handler.contains("settings: syncSettingsDraftFromForm()"),
        "Save Settings must submit the accumulated draft, not only the visible tab state"
    );
    assert!(
        sync_helper.contains("settingsDraft = readSettings();")
            && !sync_helper.contains("return settingsDraft || {};"),
        "Save Settings must fall back to a full form snapshot, not {{}}, when settings are not loaded yet"
    );
    assert!(
        js.contains("settings-page\").addEventListener(\"input\", () => syncSettingsDraftFromForm())")
            && js.contains("settings-page\").addEventListener(\"change\", () => syncSettingsDraftFromForm())"),
        "settings form edits must continuously refresh the draft before async tab renderers repaint controls"
    );
    assert!(
        js.contains("const audio = settingsFormSource().audio || defaultAudioSettings();")
            && js.contains("const selected = settingsFormSource().video_encoder || \"auto\";")
            && js.contains("function captureSettingsValue(settings = settingsFormSource())"),
        "async settings renderers must use the draft as their source while settings are being edited"
    );

    for renderer in [
        "function renderCaptureTargetSelect()",
        "function renderAudioDeviceSelects()",
        "function renderVideoEncoderSelect()",
    ] {
        let start = js.find(renderer).expect("settings option renderer");
        let end = js[start + renderer.len()..]
            .find("\nfunction ")
            .map(|offset| start + renderer.len() + offset)
            .expect("next function follows renderer");
        let body = &js[start..end];
        assert!(
            !body.contains("syncSettingsDraftFromForm()"),
            "settings option renderers must not re-read stale DOM state while fillSettings is repainting"
        );
    }
}

#[test]
fn timeline_navigator_and_zoom_controls_are_wired() {
    let html = index_html();
    let js = main_js();
    let css = styles_css();

    // The whole-clip navigator sits between the ruler and the export row.
    let metadata_panel = html
        .find("id=\"game-metadata-panel\"")
        .expect("metadata panel");
    let metadata_fields = html
        .find("id=\"game-metadata-fields\"")
        .expect("metadata fields");
    let trim_toggle = html.find("id=\"trim-mode-toggle\"").expect("trim toggle");
    let trim_action_panel = html
        .find("id=\"trim-action-panel\"")
        .expect("trim action panel");
    let timeline_footer_row = html
        .find("class=\"timeline-footer-row\"")
        .expect("timeline footer row");
    let timeline_stack = html
        .find("class=\"timeline-stack\"")
        .expect("timeline stack");
    let timeline_main = html.find("class=\"timeline-main\"").expect("timeline main");
    let timeline = html.find("id=\"timeline\"").expect("timeline");
    let marker_layer = html.find("id=\"marker-layer\"").expect("marker layer");
    let ruler = html.find("id=\"ruler\"").expect("ruler");
    let audio_track_panel = html
        .find("id=\"audio-track-panel\"")
        .expect("audio track panel");
    let export_row = html.find("class=\"export-row\"").expect("export row");
    assert!(
        metadata_panel < metadata_fields
            && metadata_fields < timeline_stack
            && timeline_stack < timeline_main
            && timeline_main < timeline
            && ruler < timeline_footer_row
            && timeline_footer_row < audio_track_panel
            && audio_track_panel < trim_action_panel
            && trim_action_panel < trim_toggle
            && trim_toggle < export_row,
        "the simple scissors trim control must sit far right in the below-timeline row beside audio tracks"
    );
    assert!(
        !html.contains("class=\"timeline-action-row\""),
        "the timeline should not reserve a separate scissors-only row"
    );
    let trim_toggle_end = html[trim_toggle..]
        .find("</button>")
        .map(|offset| trim_toggle + offset)
        .expect("trim toggle closes");
    let trim_toggle_markup = &html[trim_toggle..trim_toggle_end];
    assert!(
        !trim_toggle_markup.contains("<span>Clip</span>"),
        "the below-timeline scissors toggle should stay icon-only so it cannot look like a second export button"
    );
    assert!(
        timeline < marker_layer
            && marker_layer < ruler
            && !html.contains("class=\"timeline-rail\""),
        "event markers must live on the timeline band above the attached time ruler"
    );

    let overview = html.find("id=\"overview\"").expect("overview");
    assert!(
        ruler < overview && overview < timeline_footer_row && timeline_footer_row < export_row,
        "the navigator minimap and below-timeline actions must sit above the export row"
    );

    // Central view setter + paint/rebuild split keep the navigator in sync, and
    // every view change routes through the pure helpers.
    for required in [
        "function applyView",
        "function paintOverview",
        "function renderOverviewMarkers",
        "function maybeFollow",
        "onOverviewPointerDown",
        "function zoomAtPlayhead",
        "function zoomToSelection",
        "zoomView(",
        "panView(",
        "setViewEdge(",
        "followView(",
        "snapTime(",
    ] {
        assert!(
            js.contains(required),
            "main.js must wire the timeline through {required}"
        );
    }

    // Navigator window, markers, and snap feedback need styles.
    assert!(
        css.contains("#overview-window") && css.contains(".ov-marker") && css.contains(".snapped"),
        "navigator window, marker ticks, and snap feedback must be styled"
    );
    let metadata_panel_rule = css_rule_body(&css, ".game-metadata-panel");
    let metadata_fields_rule = css_rule_body(&css, ".game-metadata-fields");
    let timeline_footer_row_rule = css_rule_body(&css, ".timeline-footer-row");
    let trim_action_panel_rule = css_rule_body(&css, ".trim-action-panel");
    let deck_status_rule = css_rule_body(&css, ".deck-status");
    let timeline_main_rule = css_rule_body(&css, ".timeline-main");
    let timeline_rule = css_rule_body(&css, "#timeline");
    let timeline_progress_rule = css_rule_body(&css, "#timeline::before");
    let marker_layer_rule = css_rule_body(&css, "#marker-layer");
    let ruler_rule = css_rule_body(&css, ".ruler");
    let ruler_tick_rule = css_rule_body(&css, ".ruler .tick.micro");
    let ruler_lab_rule = css_rule_body(&css, ".ruler .lab");
    let marker_glyph_rule = css_rule_body(&css, ".marker .glyph");
    let marker_image_rule = css_rule_body(&css, ".marker .glyph.img");
    assert!(
        css_decl_value(metadata_panel_rule, "grid-template-columns").is_none()
            && css_decl_value(metadata_fields_rule, "display") == Some("flex")
            && css_decl_value(timeline_footer_row_rule, "display") == Some("flex")
            && css_decl_value(timeline_footer_row_rule, "border-top").is_some()
            && css_decl_value(trim_action_panel_rule, "display") == Some("flex")
            && css_decl_value(trim_action_panel_rule, "justify-content") == Some("flex-end")
            && css_decl_value(deck_status_rule, "margin-left") == Some("auto")
            && css_decl_value(trim_action_panel_rule, "border-top").is_none()
            && css_decl_value(timeline_main_rule, "position").is_some()
            && css_decl_value(timeline_main_rule, "border") == Some("0")
            && css_decl_value(timeline_main_rule, "overflow").is_some()
            && css_decl_value(timeline_rule, "position").is_some()
            && css_decl_value(timeline_rule, "border") == Some("0")
            && css_decl_value(timeline_rule, "background") == Some("transparent")
            && css_decl_value(timeline_progress_rule, "background").is_some()
            && !css.contains("#timeline::after")
            && css_decl_value(marker_layer_rule, "position").is_some()
            && css_decl_value(marker_layer_rule, "pointer-events").is_some()
            && css_decl_value(ruler_rule, "position").is_some()
            && css_decl_value(ruler_rule, "border") == Some("0")
            && css_decl_value(ruler_tick_rule, "height").is_some()
            && css_decl_value(ruler_lab_rule, "position").is_some()
            && css_decl_value(marker_glyph_rule, "width").is_some()
            && css_decl_value(marker_glyph_rule, "height").is_some()
            && css_decl_value(marker_image_rule, "mask").is_some()
            && css_decl_value(marker_image_rule, "filter")
                .is_some_and(|value| value.contains("drop-shadow")),
        "event markers must sit on a borderless timeline band above a dense attached ruler"
    );
    assert!(
        css_decl_value(css_rule_body(&css, "#trim-mode-toggle"), "display") == Some("inline-flex")
            && css_decl_value(css_rule_body(&css, "#trim-mode-toggle"), "color") == Some("#ffffff"),
        "the simple scissors trim control must read as a compact below-timeline action"
    );
    assert!(
        css_decl_value(css_rule_body(&css, "#trim-mode-toggle"), "position").is_some()
            && css_decl_value(css_rule_body(&css, "#trim-mode-toggle"), "color") == Some("#ffffff"),
        "the simple scissors trim control must stay high contrast"
    );
    assert!(
        css_decl_value(
            css_rule_body(&css, "#trim-mode-toggle.active"),
            "background"
        )
        .is_some(),
        "the moved scissors button must still show active trim state outside the deck"
    );
    let render_metadata_panel = js
        .split("function renderGameMetadataPanel")
        .nth(1)
        .and_then(|rest| rest.split("function clipGalleryCardPreview").next())
        .expect("metadata panel renderer");
    assert!(
        render_metadata_panel.contains("if (!clip) {")
            && render_metadata_panel.contains("panel.hidden = true;")
            && !render_metadata_panel.contains("panel.hidden = legacyTimelineEnabled();"),
        "the metadata bar should return to metadata-only visibility"
    );
    let timeline_preference = js
        .split("function applyTimelineEditorPreference")
        .nth(1)
        .and_then(|rest| rest.split("function setSimpleTrimMode").next())
        .expect("timeline preference function");
    assert!(
        timeline_preference.contains("$(\"trim-action-panel\").hidden = legacy;"),
        "legacy timeline mode should hide the below-timeline scissors strip"
    );
    assert!(
        css.contains(".deck.simple-timeline:not(.simple-trim-active) #export-clip")
            && css.contains(".deck.simple-timeline:not(.simple-trim-active) .trim-readout"),
        "the export clip action must remain scoped to the deck trim-mode state"
    );
    assert!(
        js.contains("const minorStep = step / 10;")
            && js.contains("const isHalf =")
            && js.contains("tick.className = isHalf ? \"tick minor\" : \"tick micro\";"),
        "the time ruler must add Outplayed-style dense ticks between major labels"
    );
    assert!(
        js.contains("MARKER_LEAD_S = 1")
            && js.contains("seekTo(markerTime - MARKER_LEAD_S, { keepGameEventSelection: true });")
            && js.contains("seekTo(m.t_s - MARKER_LEAD_S);"),
        "clicking timeline and event-rail markers must start one second before the event"
    );
    assert!(
        !css.contains(".marker-death .glyph.img") && !css.contains("190% no-repeat"),
        "normalized marker PNGs must not need per-kind timeline mask scaling"
    );
    assert!(
        css.contains(".marker .glyph.img")
            && css.contains("mask: var(--marker-img) center / contain no-repeat;\n  filter:\n    drop-shadow(1px 0 0 rgba(var(--scrim-a-rgb), 0.9))"),
        "timeline marker image glyphs must use the same black alpha-outline as event rail icons"
    );
}

#[test]
fn timeline_marker_pngs_have_matching_alpha_height() {
    let marker_asset_dirs = [
        "ui/assets/markers",
        "plugin-seeds/league_of_legends/assets/markers",
    ];
    let marker_names = [
        "assist.png",
        "baron.png",
        "death.png",
        "dragon.png",
        "kill.png",
        "turret.png",
    ];

    for asset_dir in marker_asset_dirs {
        for name in marker_names {
            let (canvas, visible) = marker_png_alpha_bounds(asset_dir, name);
            assert_eq!(
                canvas,
                (320, 320),
                "{asset_dir}/{name} canvas must match the other timeline markers"
            );
            assert_eq!(
                visible.1, 280,
                "{asset_dir}/{name} visible alpha height must match the other timeline markers"
            );
        }
    }

    let css = styles_css();
    assert!(
        !css.contains(".game-event-rail ol button.marker-kill .game-event-kind-icon img")
            && !css.contains(".game-event-rail ol button.marker-death .game-event-kind-icon img"),
        "normalized marker PNGs must not need per-kind event rail image sizing"
    );
}

#[test]
fn league_event_rail_pngs_have_matching_alpha_height() {
    let event_rail_icon_names = [
        "baron.png",
        "death.png",
        "dragon.png",
        "kill.png",
        "turret.png",
    ];

    for name in event_rail_icon_names {
        let (canvas, visible) =
            marker_png_alpha_bounds("plugin-seeds/league_of_legends/assets/event-rail", name);
        assert_eq!(
            canvas,
            (320, 320),
            "league event rail {name} canvas must match the other match event icons"
        );
        assert_eq!(
            visible.1, 280,
            "league event rail {name} visible alpha height must match the other match event icons"
        );
    }
}

#[test]
fn league_event_rail_minion_actor_pngs_are_square_portraits() {
    for name in ["minion-100.png", "minion-200.png"] {
        assert_eq!(
            png_dimensions("plugin-seeds/league_of_legends/assets/event-rail", name),
            (128, 128),
            "league event rail {name} must stay a square portrait for non-player actor slots"
        );
    }
}

#[test]
fn no_native_browser_dialogs() {
    let js = main_js();
    let css = styles_css();
    // window.confirm/alert render browser chrome ("tauri.localhost says") —
    // use the in-app #confirm-dialog instead.
    for banned in ["confirm(", "alert("] {
        assert!(
            !js.contains(banned),
            "main.js must not call native {banned}…) — use the in-app dialog"
        );
    }

    assert!(
        js.contains("document.addEventListener(\"contextmenu\", (ev) => {")
            && js.contains("ev.preventDefault();")
            && js.contains("showClipContextMenu(ev, c)")
            && js.contains("showCloudClipContextMenu(ev, entry)")
            && js.contains("$(\"clip-menu-play\").addEventListener(\"click\"")
            && js.contains("$(\"clip-menu-open-cloud-page\").addEventListener(\"click\"")
            && js.contains("$(\"clip-menu-copy-cloud-link\").addEventListener(\"click\"")
            && js.contains("$(\"clip-menu-upload\").addEventListener(\"click\"")
            && js.contains("$(\"clip-menu-rename\").addEventListener(\"click\"")
            && js.contains("$(\"clip-menu-rename-file\").addEventListener(\"click\"")
            && js.contains("$(\"clip-menu-delete\").addEventListener(\"click\"")
            && js.contains("function beginClipRename")
            && js.contains("function openRenameFileDialog")
            && js.contains("await invoke(\"rename_clip\"")
            && js.contains("await invoke(\"rename_clip_file\"")
            && app_rs().contains("crate::library::rename_clip")
            && app_rs().contains("crate::library::rename_clip_file")
            && index_html().contains("id=\"clip-menu-rename-file\"")
            && index_html().contains("id=\"rename-file-dialog\"")
            && index_html().contains("id=\"rename-file-input\"")
            && js.contains("clipKind(c)")
            && !js.contains("clipKind(c.name)")
            && css.contains(".clip-title-edit")
            && css.contains(".context-menu button[hidden]")
            && css.contains("#rename-file-dialog")
            && css.contains(".context-menu button.danger-text"),
        "native context menus must be suppressed and library rows must expose an app-owned clip menu"
    );
}

#[test]
fn controls_have_custom_range_and_scrollbar_skin() {
    let css = styles_css();
    let js = main_js();

    assert!(
        css.contains("::-webkit-slider-thumb") && css.contains("::-moz-range-thumb"),
        "range inputs should use Clipline slider styling instead of native defaults"
    );
    assert!(
        css.contains("::-webkit-scrollbar-thumb") && css.contains("scrollbar-color"),
        "scrollable areas should use the app scrollbar styling"
    );
    assert!(
        css.contains("--range-progress") && js.contains("syncRangeProgress"),
        "slider fill must stay synced to the current value"
    );
    assert!(
        css.contains("background-position: right 12px center")
            && css.contains("-webkit-appearance: none"),
        "select arrows should use the app inset instead of the native edge-hugging arrow"
    );
}

#[test]
fn card_kind_badges_keep_text_optically_centered() {
    let css = styles_css();
    let js = main_js();

    assert!(
        js.matches("kindLabel.className = \"card-kind-label\"")
            .count()
            >= 2,
        "card kind badge labels must be addressable separately from their icons"
    );

    let label_rule = css_rule_body(&css, ".card-kind-label");
    assert_eq!(
        css_decl_value(label_rule, "display"),
        Some("block"),
        "badge text should use a tight block line box inside the flex pill"
    );
    assert_eq!(
        css_decl_value(label_rule, "line-height"),
        Some("1"),
        "badge text should not inherit loose font line-height metrics"
    );

    assert!(
        !css.contains(".card-kind.session .card-kind-label"),
        "badge label centering should come from shared text metrics, not a session-only nudge"
    );
}

#[test]
fn shell_shows_live_memory_usage() {
    let html = index_html();
    let js = main_js();
    let css = styles_css();

    assert!(
        html.contains("id=\"memory-usage\"") && html.contains("Using -- RAM"),
        "sidebar chrome must include the RAM indicator placeholder"
    );
    assert!(
        html.contains("id=\"memory-children\""),
        "child-process memory needs its own element so the app figure stays unblended"
    );
    assert!(
        js.contains("memory_status"),
        "memory indicator must use the backend sampler"
    );
    assert!(
        js.contains("if (!document.hidden && captureForegroundWork()) refreshMemoryUsage()")
            && js.contains("visibilitychange"),
        "memory polling must pause while hidden and refresh when visible again"
    );
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let app = fs::read_to_string(source_root.join("app.rs")).expect("read app.rs");
    assert!(
        app.contains("async fn memory_status(")
            && app.contains("State<'_, crate::memory::MemorySampler>"),
        "memory_status must use the managed asynchronous sampler"
    );
    let memory = fs::read_to_string(source_root.join("memory.rs")).expect("read memory.rs");
    assert!(
        memory.contains("spawn_blocking") && memory.contains("tokio::sync::Mutex"),
        "the process-tree walk must run on the blocking pool and coalesce callers"
    );
    assert!(
        css.contains(".memory-usage") && css.contains("font-variant-numeric: tabular-nums"),
        "memory usage should have stable numeric styling in the top-left chrome"
    );
    assert!(
        css.contains(".memory-children[hidden] { display: none; }"),
        "a display rule on the child line would otherwise defeat the [hidden] attribute"
    );
    assert!(
        js.contains("process_private_working_set_bytes")
            && js.contains("children_private_working_set_bytes"),
        "the meter must read the split fields, not the blended tree total"
    );
}

#[test]
fn gallery_header_shows_library_storage_usage() {
    let html = index_html();
    let js = main_js();
    let css = styles_css();

    assert!(
        html.contains("id=\"gallery-count\"") && html.contains("id=\"gallery-storage-used\""),
        "gallery header should include storage usage beside the clip count"
    );
    assert!(
        js.contains("$(\"gallery-storage-used\").textContent")
            && js.contains("const quotaGb = s.quota_bytes == null")
            && js.contains("fmtLibraryStorageUsage(s.total_bytes, quotaGb)"),
        "refreshStorage should render total library bytes and configured quota from storage_status into the gallery header"
    );
    assert!(
        css.contains(".gallery-storage-used"),
        "storage usage should have gallery header metadata styling"
    );
}

#[test]
fn gallery_renders_one_bounded_page_for_local_and_cloud_sources() {
    let html = index_html();
    let library = read_ui_js("library.js");
    let app_core = read_ui_js("app-core.js");
    let css = styles_css();

    for required in [
        "id=\"gallery-pagination\"",
        "id=\"gallery-page-prev\"",
        "id=\"gallery-page-label\"",
        "id=\"gallery-page-next\"",
    ] {
        assert!(
            html.contains(required),
            "bounded gallery markup must include `{required}`"
        );
    }
    assert!(
        app_core.contains("galleryPageState = GalleryWindowCore.initialState()")
            && app_core.contains("POSTER_CACHE_LIMIT = 120"),
        "gallery state must start from the pure window core with a finite poster cache"
    );

    let local = js_function_body(&library, "renderClips");
    assert!(
        local.contains("GalleryWindowCore.windowGroups(groups, galleryPageState)")
            && local.contains("for (const group of page.groups)")
            && local.contains("for (const c of group.items)"),
        "local rendering must only create cards from the current grouped page"
    );
    let cloud = js_function_body(&library, "renderCloudClips");
    assert!(
        cloud.contains("GalleryWindowCore.windowItems(filtered, galleryPageState)")
            && cloud.contains("for (const entry of page.items)"),
        "cloud rendering must only create cards from the current page"
    );
    assert!(
        library.contains("GalleryWindowCore.updateState(galleryPageState")
            && library.contains("function groupedGalleryIdentity(")
            && library.contains("function changeGalleryPage(delta)"),
        "source, filter, grouping, and data identities must reset or clamp pagination"
    );
    let identity = js_function_body(&library, "groupedGalleryIdentity");
    assert!(
        identity.contains("firstItem")
            && identity.contains("lastItem")
            && identity.contains("maxModifiedUnix")
            && !identity.contains("for ("),
        "page identity must use precomputed collection boundaries without rescanning groups or clips"
    );
    assert!(
        js_function_body(&library, "selectAllVisible")
            .contains("selectedClipPaths = new Set(selectedClipPaths);"),
        "selecting a page must retain path-keyed selections made on other pages"
    );
    for required in [
        ".gallery-pagination",
        ".gallery-pagination[hidden] { display: none; }",
        "grid-template-rows: auto auto minmax(0, 1fr) auto;",
    ] {
        assert!(
            css.contains(required),
            "pagination must have a stable, non-scrolling layout through `{required}`"
        );
    }
}

#[test]
fn gallery_releases_off_page_images_and_bounds_poster_state() {
    let library = read_ui_js("library.js");
    let cloud = read_ui_js("cloud.js");

    let release = js_function_body(&library, "releaseGalleryRoot");
    assert!(
        release.contains("img.removeAttribute(\"src\");")
            && release.contains("root.replaceChildren();"),
        "changing pages/sources must release decoded images and detached card DOM"
    );
    let begin = js_function_body(&library, "beginBoundedGalleryRender");
    assert!(
        begin.contains("posterObserver.disconnect()")
            && begin.contains("posterWorkQueue = [];")
            && begin.contains("releaseGalleryRoot($(\"gallery-grid\"))")
            && begin.contains("releaseGalleryRoot($(\"cloud-gallery-grid\"))"),
        "each render must invalidate poster observation and clear both source roots"
    );

    let local_card = js_function_body(&library, "clipCard");
    let cloud_card = js_function_body(&library, "cloudClipCard");
    assert!(
        local_card.contains("observePoster(c.path, thumb)")
            && !local_card.contains("posterCacheGet(")
            && cloud_card.contains("observeCloudThumbnail(entry, thumb)"),
        "cached and uncached local/cloud images must use the same viewport gate"
    );
    let set_cache = js_function_body(&library, "posterCacheSet");
    assert!(
        set_cache.contains("GalleryWindowCore.cacheSet(")
            && set_cache.contains("POSTER_CACHE_LIMIT")
            && set_cache.contains("posterUnavailableUntil")
            && library.contains("GalleryWindowCore.cacheGet(posterCache, key)"),
        "poster URL and unavailable entries must use the pure tested bounded LRU helper"
    );
    let local_paths = js_function_body(&library, "localClipPaths");
    let prune_local = js_function_body(&library, "pruneLocalPosterCache");
    let load_local = js_function_body(&library, "loadCardPoster");
    assert!(
        local_paths.contains("new Set()")
            && local_paths.contains("GalleryWindowCore.clipPathKey")
            && prune_local.contains("paths.has(GalleryWindowCore.clipPathKey(key))")
            && load_local.contains("localClipPaths(clipsCache).has(")
            && !load_local.contains("clipsCache.some("),
        "poster membership must use one normalized path index instead of scanning every clip per card"
    );
    for required in [
        "function pruneLocalPosterCache(clips)",
        "function pruneCloudPosterCache(entries)",
        "function clearCloudPosterCache()",
        "pruneLocalPosterCache(clipsCache);",
        "POSTER_WORK_LIMIT = 2",
    ] {
        assert!(
            library.contains(required),
            "poster memory/work bounds must include `{required}`"
        );
    }
    assert!(
        cloud.contains("clearCloudPosterCache();")
            && cloud.contains("pruneCloudPosterCache(cloudClipsCache);")
            && cloud.contains("${CLOUD_POSTER_CACHE_PREFIX}${cloudAccountKey()}"),
        "cloud poster keys must be account-scoped and pruned on reset/refresh"
    );
}

#[test]
fn library_has_cloud_source_tab() {
    let html = index_html();
    let js = main_js();
    let css = styles_css();

    for required in [
        "id=\"gallery-source-tabs\"",
        "data-gallery-source=\"local\"",
        "data-gallery-source=\"cloud\"",
        "id=\"cloud-gallery-grid\"",
    ] {
        assert!(
            html.contains(required),
            "library markup must include cloud source tab contract `{required}`"
        );
    }
    for required in [
        "gallerySource = \"local\"",
        "function renderCloudClips()",
        "function cloudLocalClipForEntry(entry)",
        "function openCloudEntryInApp(entry)",
        "function cloudRecordUploaded(record)",
        "function cloudShareUrl(record)",
        "function showCloudClipContextMenu(ev, entry)",
        "function observeCloudThumbnail(entry, thumb)",
        "function loadCloudThumbnail(entry, thumb)",
        "cloudThumbnailInflight = new Map()",
        "const request = { type: \"cloud-thumbnail\", entry }",
        "posterQueue.set(thumb, request)",
        "cloudClipsCache = []",
        "function loadCloudClips",
        "if (gallerySource === \"cloud\") renderCloudClips();",
        "if (cloudClipsError && !force) return;",
        "error.className = \"gallery-empty cloud-error\"",
        "function isCloudOnlyReviewClip(clip = currentClip)",
        "function syncReviewLocalActions()",
        "invoke(\"list_cloud_clips\")",
        "invoke(\"cache_cloud_clip_media\"",
        "invoke(\"cloud_clip_thumbnail\"",
        "invoke(\"open_cloud_clip\"",
        "PlayerCore.cloudLibraryEntries",
        "localClip ? clipCard(localClip) : cloudClipCard(entry)",
        "showCloudClipContextMenu(ev, entry)",
        "$(\"clip-menu-open-cloud-page\").disabled = !entry.remote_clip_id",
        "$(\"clip-menu-copy-cloud-link\").hidden = !cloudShareUrl(entry)",
        "entry.remote_url || \"No public share link\"",
        "session: entry.remote_clip_id",
        "Open cloud page — no public share link",
        "$(\"cloud-gallery-grid\")",
        "querySelectorAll(\"#gallery-source-tabs .source-tab\")",
    ] {
        assert!(
            js.contains(required),
            "main.js must wire cloud library behavior through `{required}`"
        );
    }
    assert!(
        !js.contains("actions.className = \"card-actions\""),
        "cloud-only cards should not render inline Play/Open/Copy buttons"
    );
    let player_core = player_core_js();
    assert!(
        player_core.contains("if (!clip || (!clip.remote_clip_id && !clip.remote_url)) continue;")
            && player_core.contains(
                "if (!record || (!record.remote_clip_id && !record.remote_url)) return false;"
            )
            && player_core.contains("remote_url: String(clip.remote_url || \"\")")
            && player_core.contains("remote_url: String(record.remote_url || \"\")"),
        "private cloud entries must remain addressable by remote identity without a share URL"
    );
    for required in [
        "POSTER_UNAVAILABLE = Symbol(\"poster unavailable\")",
        "function markPosterUnavailable(path)",
        ".card-markers",
        "img.addEventListener(\"error\", () => {",
        "img.remove();",
        "if (onError) onError();",
        "posterCacheSet(path, POSTER_UNAVAILABLE);",
        "if (cached === POSTER_UNAVAILABLE) return Promise.resolve();",
        "POSTER_UNAVAILABLE_RETRY_MS = 30_000",
        "Date.now() >= retryAt",
        "if (!isForegroundWorkCurrent(lifecycleWork)) return;",
        "GalleryWindowCore.posterRuntimeUnavailable(error)",
        "function retryUnavailablePosters()",
        "value === POSTER_UNAVAILABLE",
        "!String(key).startsWith(CLOUD_POSTER_CACHE_PREFIX)",
        "posterCacheDelete(key);",
        "loadCardPoster(path, thumb)",
        "observePoster(c.path, thumb)",
        "insertThumbMedia(thumb, makePosterImg(cached",
        "insertThumbMedia(thumb, makePosterImg(url))",
    ] {
        assert!(
            js.contains(required),
            "local clip thumbnails must safely cache poster failures and preserve overlays through `{required}`"
        );
    }
    for forbidden in [
        "function makePosterFallbackVideo(",
        "function showPosterFallback(",
        "video.className = \"card-thumb-img\"",
        "img.addEventListener(\"error\", () => onError && onError())",
        "thumb.appendChild(makePosterImg(cached))",
    ] {
        assert!(
            !js.contains(forbidden),
            "thumbnail fallbacks must not keep source media open or bypass overlay-safe insertion via `{forbidden}`"
        );
    }
    for required in [
        "id=\"poster-runtime-warning\"",
        "id=\"poster-runtime-install\"",
        "optional FFmpeg component",
    ] {
        assert!(
            html.contains(required),
            "missing thumbnail runtime must expose actionable UI through `{required}`"
        );
    }
    assert!(
        js.contains("$(\"poster-runtime-install\").addEventListener(\"click\"")
            && js.contains("installFfmpegForPosters()"),
        "the thumbnail runtime warning must expose the managed FFmpeg installer"
    );
    assert!(
        css.contains(".poster-runtime-warning[hidden] { display: none; }")
            && css.contains("pointer-events: auto;"),
        "the actionable runtime warning must obey hidden state and accept clicks"
    );
    for required in [
        ".gallery-source-tabs",
        ".source-tab.active",
        ".cloud-gallery-grid",
        ".cloud-card",
        ".cloud-card-placeholder > svg",
        ".gallery-empty.cloud-error",
    ] {
        assert!(
            css.contains(required),
            "cloud library tab should have stable styling for `{required}`"
        );
    }
    assert!(
        app_rs().contains("crate::cloud::list_cloud_clips"),
        "native command registry must expose list_cloud_clips for the Cloud library tab"
    );
    assert!(
        app_rs().contains("crate::cloud::open_cloud_clip"),
        "native command registry must expose open_cloud_clip for Cloud card links"
    );
}

#[test]
fn cloud_upload_state_matches_legacy_windows_canonical_paths() {
    let js = main_js();
    for required in [
        "function cloudUploadRecordForPath(path)",
        "PlayerCore.sameClipPath(record.path, path)",
        "PlayerCore.sameClipPath(clip.path, entry.path)",
        "PlayerCore.sameClipPath(record.path, oldPath)",
    ] {
        assert!(
            js.contains(required),
            "cloud UI path pairing must use equivalent Windows paths through `{required}`"
        );
    }
}

#[test]
fn cloud_library_loader_guards_every_async_result_and_force_supersedes() {
    let cloud = read_ui_js("cloud.js");
    let loader = js_function_body(&cloud, "loadCloudClips");
    assert!(loader.contains("cloudClipsLoading && !force"));
    assert!(loader.contains("cloudClipsRequestGate.begin(accountKey)"));
    assert!(loader.contains("cloudClipsRequestGate.isCurrent(request, cloudAccountKey())"));
    assert!(!loader.contains("if (cloudClipsLoading) return"));

    let await_result = loader.find("await invoke(\"list_cloud_clips\")").unwrap();
    let success_guard = loader[await_result..]
        .find("if (!isCurrent()) return;")
        .map(|offset| await_result + offset)
        .unwrap();
    let success_publish = loader.find("cloudClipsCache = result").unwrap();
    let success_loaded = loader.find("cloudClipsLoaded = true;").unwrap();
    let catch_start = loader.find("} catch (error) {").unwrap();
    assert!(await_result < success_guard);
    assert!(success_guard < success_publish);
    assert!(success_publish < success_loaded);
    assert!(success_loaded < catch_start);

    let error_guard = loader[catch_start..]
        .find("if (!isCurrent()) return;")
        .map(|offset| catch_start + offset)
        .unwrap();
    let error_publish = loader.find("cloudClipsError = String(error);").unwrap();
    let finally_start = loader.find("} finally {").unwrap();
    assert!(catch_start < error_guard);
    assert!(error_guard < error_publish);
    assert!(error_publish < finally_start);

    let finally_guard = loader[finally_start..]
        .find("if (!isCurrent()) return;")
        .map(|offset| finally_start + offset)
        .unwrap();
    let loading_clear = loader.find("cloudClipsLoading = false;").unwrap();
    let final_render = loader[loading_clear..]
        .find("if (gallerySource === \"cloud\") renderClips();")
        .map(|offset| loading_clear + offset)
        .unwrap();
    assert!(finally_start < finally_guard);
    assert!(finally_guard < loading_clear);
    assert!(loading_clear < final_render);

    let html = index_html();
    let cloud_core = html.find("src=\"cloud-core.js\"").unwrap();
    let app_core = html.find("src=\"app-core.js\"").unwrap();
    assert!(cloud_core < app_core);
}

#[test]
fn cloud_tab_click_forces_authoritative_refresh() {
    let main = read_ui_js("main.js");
    let click_start = main
        .find("$(\"gallery-source-tabs\").addEventListener(\"click\"")
        .expect("gallery source tabs must have a click handler");
    let click_end = main[click_start..]
        .find("$(\"gallery-select-toggle\")")
        .map(|offset| click_start + offset)
        .expect("gallery source handler must precede the selection handler");
    let click = &main[click_start..click_end];
    assert!(click.contains("loadCloudClips({ force: true })"));
    assert!(
        click.find("loadCloudClips({ force: true })") < click.find("renderClips()"),
        "Cloud selection must start its forced request before generic rendering can request cached data"
    );

    let cloud = read_ui_js("cloud.js");
    let records = js_function_body(&cloud, "cloudLibraryRecords");
    assert!(
        records.contains("cloudListAuthoritative")
            && records.contains("cloudClipsLoaded")
            && records.contains("cloudClipsLoading")
            && records.contains("cloudClipsCache,"),
        "the renderer must distinguish an authoritative server response from an uninitialized cache"
    );
    assert!(
        !records.contains("Boolean(cloudClipsError)"),
        "a failed refresh must preserve cached completed uploads"
    );
}

#[test]
fn rail_profile_identity_change_resets_and_refetches_cloud_library() {
    let cloud = read_ui_js("cloud.js");
    let sync = js_function_body(&cloud, "syncRailProfile");
    assert!(
        sync.contains("const lifecycleWork = captureForegroundWork();")
            && sync.contains("refreshRailProfileIdentity(key, lifecycleWork)")
            && sync.contains("loadRailProfileAvatar(key, name, lifecycleWork)"),
        "profile and avatar requests must capture the same foreground lifecycle generation"
    );
    let refresh = js_function_body(&cloud, "refreshRailProfileIdentity");
    assert!(
        refresh.contains("if (!lifecycleWork) return;")
            && refresh.contains("!isForegroundWorkCurrent(lifecycleWork)"),
        "a profile response captured before backgrounding must not mutate account or DOM state"
    );
    let capture = refresh
        .find("const previousAccountKey = cloudAccountKey()")
        .expect("profile refresh must capture the account before mutation");
    let mutation = refresh
        .find("cloud.connected_user_id = profile.user_id || cloud.connected_user_id")
        .expect("profile refresh must update the canonical connected user id");
    let identity_change = refresh
        .find(concat!(
            "if (cloudAccountKey() !== previousAccountKey) {\n",
            "      resetCloudClipsCache();\n",
            "      if (gallerySource === \"cloud\") loadCloudClips({ force: true });\n",
            "    }",
        ))
        .expect("identity change must reset and force-refetch the active cloud gallery");

    assert!(capture < mutation && mutation < identity_change);

    let avatar = js_function_body(&cloud, "loadRailProfileAvatar");
    assert!(
        avatar.contains("if (!lifecycleWork) return;")
            && avatar.contains("!isForegroundWorkCurrent(lifecycleWork)")
            && avatar.contains("request !== railProfileAvatarRequest")
            && avatar.contains("key !== railProfileAvatarKey"),
        "an avatar response must satisfy lifecycle, request, and account identity gates before attaching its data URL"
    );
}

#[test]
fn games_ui_wires_detection_commands() {
    let js = main_js();

    for required in [
        "await invoke(\"list_game_plugins\")",
        "await invoke(\"list_game_windows\")",
        "listen(\"game-detection\"",
        "if (captureForegroundWork()) loadGamePlugins();",
        "var detectedGameCandidates = []",
        "var selectedDetectedGameIds = new Set()",
        "var detectedGamesScanId = 0",
        "await invoke(\"detect_installed_games\", { existingCustomGames: customGames })",
        "const scanId = ++detectedGamesScanId",
        "$(\"detected-games-dialog\").showModal()",
        "scanId !== detectedGamesScanId",
        "!$(\"detected-games-dialog\").open",
        "detectedGamesScanId += 1",
        "const addableKeys = new Set(addable.map(detectedGameKey))",
        "selectedDetectedGameIds = new Set([...selectedDetectedGameIds].filter((key) => addableKeys.has(key)))",
        "function uniqueCustomGameId",
        "const usedIds = new Set(customGames.map((game) => game.id))",
        ".map((candidate) => customGameFromDetectedCandidate(candidate, usedIds))",
        "renderGamePlugins",
        "renderCustomGames",
        "refreshGameWindows",
        "renderDetectedGames",
        "showDetectedGamesDialog",
        "addSelectedDetectedGames",
        "$(\"add-custom-game\").addEventListener(\"click\", showGameWindowPicker)",
        "$(\"detect-games\").addEventListener(\"click\", showDetectedGamesDialog)",
        "$(\"add-detected-games\").addEventListener(\"click\", addSelectedDetectedGames)",
        "$(\"cancel-detected-games\").addEventListener(\"click\", hideDetectedGamesDialog)",
        "$(\"refresh-game-windows\").addEventListener(\"click\", refreshGameWindows)",
        "$(\"cancel-game-picker\").addEventListener(\"click\", hideGameWindowPicker)",
    ] {
        assert!(
            js.contains(required),
            "main/settings JS must wire detected games workflow through {required}"
        );
    }
    for function in [
        "refreshGameWindows",
        "showDetectedGamesDialog",
        "addCustomGameFromWindow",
    ] {
        let body = js_function_body(&js, function);
        assert!(
            body.contains("captureForegroundWork()")
                && body.contains("isForegroundWorkCurrent(lifecycleWork)"),
            "{function} must reject a completion from before a background transition"
        );
    }
    let background_settings = js_function_body(&js, "releaseBackgroundSettingsUi");
    for required in [
        "gameWindowsScanId += 1;",
        "gameWindows = [];",
        "detectedGamesScanId += 1;",
        "detectedGameCandidates = [];",
        "selectedDetectedGameIds = new Set();",
    ] {
        assert!(
            background_settings.contains(required),
            "background settings teardown must invalidate and clear `{required}`"
        );
    }

    for required in ["fn detect_installed_games", "detect_installed_games,"] {
        assert!(
            app_rs().contains(required),
            "native command registry must expose detected game scan through {required}"
        );
    }

    for required in [
        ".detected-game,",
        "#detected-games-dialog,",
        "#game-window-picker-dialog",
    ] {
        assert!(
            styles_css().contains(required),
            "styles.css must style detected games workflow through {required}"
        );
    }
}

#[test]
fn deck_status_success_toasts_auto_clear() {
    let js = main_js();

    assert!(
        js.contains("DECK_STATUS_TOAST_MS")
            && js.contains("deckStatusToastTimer")
            && js.contains("function setDeckStatus(message, { transient = false } = {})"),
        "deck status messages should flow through a helper that can schedule transient toasts"
    );
    assert!(
        js.contains("window.setTimeout(() => {")
            && js.contains("if ($(\"deck-status\").textContent === message)"),
        "transient deck status toasts should clear themselves without erasing newer messages"
    );

    for required in [
        "setDeckStatus(audioSelectionLabel(currentClip), { transient: true })",
        "setDeckStatus(\"clip renamed\", { transient: true })",
        "setDeckStatus(`exported ${exportedLabel} · keyframe-aligned ${fmtTenths(exported.aligned_start_s)} – ${fmtTenths(exported.aligned_end_s)}`, { transient: true })",
        "setDeckStatus(original ? \"original clip copied\" : \"shareable clip copied\", { transient: true })",
        "setDeckStatus(\"cloud link copied\", { transient: true })",
        "setDeckStatus(\"cloud upload ready\", { transient: true })",
    ] {
        assert!(
            js.contains(required),
            "success toast should auto-clear via `{required}`"
        );
    }

    for required in [
        "setDeckStatus(\"switching audio tracks...\")",
        "setDeckStatus(\"renaming clip...\")",
        "setDeckStatus(\"exporting…\")",
        "setDeckStatus(\"uploading to cloud...\")",
        "setDeckStatus(\"cloud upload processing\")",
    ] {
        assert!(
            js.contains(required),
            "progress status should stay explicit via `{required}`"
        );
    }
}

#[test]
fn ffmpeg_capability_cache_is_replaceable_after_managed_install() {
    let service = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/service.rs"))
        .expect("read service.rs");
    let install =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ffmpeg_install.rs"))
            .expect("read ffmpeg_install.rs");
    assert!(
        service.contains("fn mft_capabilities_cached(")
            && service.contains("fn ffmpeg_capabilities_cached(")
            && service.contains("fn refresh_ffmpeg_encoder_capabilities(")
            && service.contains("fn ffmpeg_capability_identity("),
        "service must split process-static MFT caps from replaceable FFmpeg caps"
    );
    assert!(
        !service.contains("static CAPS: OnceLock<Vec<EncoderCapability>>"),
        "combined OnceLock encoder cache must be gone"
    );
    assert!(
        install.contains("refresh_ffmpeg_encoder_capabilities()")
            && install.contains("encoders-changed"),
        "managed ensure must refresh FFmpeg caps and notify the UI"
    );
}

#[test]
fn ffmpeg_install_commands_are_native_and_queryable() {
    let install =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ffmpeg_install.rs"))
            .expect("read ffmpeg_install.rs");
    let app = app_rs();
    let manifest =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("ffmpeg-runtime.json"))
            .expect("read ffmpeg-runtime.json");

    for required in [
        "enum FfmpegInstallState",
        "Downloading {",
        "fn ffmpeg_runtime_status(",
        "async fn ensure_ffmpeg_runtime(",
        "fn cancel_ffmpeg_runtime_install(",
        "fn sweep_abandoned_staging(",
        "archive_size",
        "FFMPEG_INSTALL_EVENT",
    ] {
        assert!(
            install.contains(required),
            "ffmpeg_install.rs must expose `{required}`"
        );
    }
    assert!(
        manifest.contains("\"archive_size\": 70103338"),
        "committed manifest must pin exact archive_size bytes"
    );
    assert!(
        app.contains("crate::ffmpeg_install::ffmpeg_runtime_status")
            && app.contains("crate::ffmpeg_install::ensure_ffmpeg_runtime")
            && app.contains("crate::ffmpeg_install::cancel_ffmpeg_runtime_install")
            && app.contains("FfmpegInstallController::default()"),
        "app must manage install controller and expose status/ensure/cancel commands"
    );
    assert!(
        app.contains("ffmpeg_staging_startup_sweep_failed"),
        "startup must sweep abandoned ffmpeg staging"
    );
}

#[test]
fn ffmpeg_install_is_reachable_from_settings_and_blocked_library_actions() {
    let html = index_html();
    let main = main_js();
    let settings = read_ui_js("settings.js");
    let library = read_ui_js("library.js");
    let review = read_ui_js("review-player.js");

    for required in [
        "id=\"ffmpeg-runtime-status\"",
        "id=\"ffmpeg-runtime-progress\"",
        "id=\"ffmpeg-runtime-install\"",
        "id=\"ffmpeg-runtime-cancel\"",
        "id=\"poster-runtime-install\"",
    ] {
        assert!(
            html.contains(required),
            "managed FFmpeg UI must expose `{required}`"
        );
    }
    for required in [
        "invoke(\"ffmpeg_runtime_status\")",
        "invoke(\"ensure_ffmpeg_runtime\")",
        "invoke(\"cancel_ffmpeg_runtime_install\")",
        "function applyFfmpegInstallSnapshot(",
    ] {
        assert!(
            settings.contains(required),
            "settings runtime controller must contain `{required}`"
        );
    }
    assert!(
        main.contains("listen(\"ffmpeg-install\"")
            && main.contains("listen(\"encoders-changed\"")
            && main.contains("installFfmpegForPosters"),
        "frontend must subscribe to native progress/capability refresh and wire poster install"
    );
    assert!(
        library.contains("showPosterRuntimeWarning")
            && review.contains("ffmpegRuntimeUnavailable(error)")
            && review.contains("ensureFfmpegRuntime("),
        "poster, audio-preview, and Copy Clip failures must offer install-and-retry"
    );
}

#[test]
fn ffmpeg_managed_runtime_verifier_is_separate_from_locate() {
    let runtime =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ffmpeg_runtime.rs"))
            .expect("read ffmpeg_runtime.rs");
    let capture = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/clipline-capture/src/ffmpeg.rs"),
    )
    .expect("read capture ffmpeg.rs");

    for required in [
        "fn verify_managed_ffmpeg_runtime(",
        "fn ffmpeg_runtime_status(",
        "struct ManagedRuntimeInfo",
        "ManagedRuntimeVerifyError",
        "fn managed_runtime_needs_repair(",
        "PROVENANCE.json",
        "manifest_sha256",
    ] {
        assert!(
            runtime.contains(required),
            "ffmpeg_runtime.rs must expose managed verifier surface `{required}`"
        );
    }
    assert!(
        capture.contains("pub fn locate()")
            && capture.contains("CLIPLINE_FFMPEG")
            && capture.contains("LOCALAPPDATA"),
        "locate() remains discovery-only and must stay in clipline-capture"
    );
    assert!(
        !capture.contains("verify_managed_ffmpeg_runtime"),
        "managed verification must not live inside locate()"
    );
}

#[test]
fn ffmpeg_capability_matrix_contracts_managed_runtime_surfaces() {
    let runtime =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ffmpeg_runtime.rs"))
            .expect("read ffmpeg_runtime.rs");
    let library = library_rs();
    let js = main_js();
    let gallery = read_ui_js("gallery-window-core.js");

    for required in [
        "enum FfmpegRequirementReason",
        "ShareableClipboardExport",
        "AudioSidecarExtract",
        "Poster",
        "SvtAv1",
        "FfmpegBackendEncoder",
        "enum FfmpegDiscoveryKind",
        "ManagedVerified",
        "ExternalUnmanaged",
        "Missing",
        "fn recording_without_ffmpeg_possible",
        "fn ffmpeg_required_for",
        "fn ensure_ffmpeg_runtime_is_noop",
        "Install the FFmpeg runtime to copy a shareable clip.",
        "Install the FFmpeg runtime to generate clip posters.",
    ] {
        assert!(
            runtime.contains(required),
            "ffmpeg_runtime.rs must expose capability-matrix surface `{required}`"
        );
    }

    assert!(
        library.contains("ffmpeg is not available for a shareable clipboard export")
            && library.contains("ffmpeg is not available for poster extraction"),
        "library paths must keep actionable missing-FFmpeg errors for share export and posters"
    );
    assert!(
        gallery.contains("posterRuntimeUnavailable")
            && gallery.contains("ffmpeg is not available for poster extraction"),
        "gallery core must recognize the poster missing-runtime error"
    );
    assert!(
        js.contains("copyClipToClipboard")
            && js.contains("copy_clip_to_clipboard"),
        "Copy Clip UI must remain the share-export entry point that will host the install affordance"
    );
    assert!(
        runtime.contains("archive_size") && runtime.contains("fn free_space_required_bytes"),
        "managed download manifest and free-space planner must carry archive_size"
    );
}

#[test]
fn clipboard_copy_distinguishes_shareable_and_original_paths() {
    let js = main_js();
    let html = index_html();
    let app = app_rs();
    let library = library_rs();

    assert!(
        library.contains("pub struct CopyClipToClipboardRequest")
            && library.contains("pub original: bool")
            && library.contains("request: CopyClipToClipboardRequest")
            && library.contains("window: tauri::WebviewWindow")
            && library.contains(".hwnd()")
            && library.contains("SetClipboardData(CF_HDROP as u32")
            && !library.contains("EmptyClipboard()"),
        "clipboard command should accept selected audio while native clipboard ownership comes from the invoking Clipline window",
    );
    assert!(
        app.contains("crate::library::copy_clip_to_clipboard"),
        "clipboard command should stay registered with Tauri"
    );
    assert!(
        js.contains("await invoke(\"copy_clip_to_clipboard\", {")
            && js.contains("request: {")
            && js.contains("path: currentClip.path")
            && js.contains("audioTrackIds: original")
            && js.contains(": clipAudioTracks(currentClip).length")
            && js.contains("selectedAudioTrackIdsForClip(currentClip)"),
        "normal copy should send selected audio while original copy should bypass audio selection"
    );
    assert!(
        js.contains("async function copyClipToClipboard(event)")
            && js.contains("const original = Boolean(event?.shiftKey);")
            && js.contains("original,")
            && js.contains("setDeckStatus(\"preparing shareable clip...\")")
            && js.contains(
                "setDeckStatus(original ? \"original clip copied\" : \"shareable clip copied\", { transient: true })"
            )
            && js.contains(
                "$(\"copy-clip\").addEventListener(\"click\", (event) => copyClipToClipboard(event));"
            ),
        "copy interaction should reserve Shift+click for the untouched original"
    );
    assert!(
        html.contains("title=\"Copy shareable clip to clipboard (Shift+click for original)\""),
        "the otherwise-hidden Shift+click behavior must be documented in the tooltip"
    );
    assert!(
        library.contains("\"share-export-v3-aac-h264-cbr8m\""),
        "share export cache must not reuse Opus or default-quality transcodes"
    );
}

#[test]
fn file_rename_reapplies_selected_audio_preview() {
    let js = main_js();
    let submit = js_function_body(&js, "submitRenameFileDialog");

    assert!(
        submit.contains("requestSelectedAudioPreview();"),
        "renaming the open source file should restore the selected audio-track preview"
    );
}

#[test]
fn app_notice_toasts_auto_clear() {
    let js = main_js();

    assert!(
        js.contains("NOTICE_TOAST_MS")
            && js.contains("noticeToastTimer")
            && js.contains("function setNotice(message, { transient = false } = {})"),
        "app-wide notices should flow through a helper that can schedule transient toasts"
    );
    assert!(
        js.contains("window.setTimeout(() => {")
            && js.contains("if ($(\"notice\").textContent === message)"),
        "transient app-wide notices should clear themselves without erasing newer messages"
    );

    for required in [
        "setNotice(\"clip renamed\", { transient: true })",
        "setNotice(\"clip deleted\", { transient: true })",
        "setNotice(s.gc_deleted",
        ": `saved ${fmtDur(s.seconds)} ${savedKind}`, { transient: true });",
    ] {
        assert!(
            js.contains(required),
            "app-wide success notice should auto-clear via `{required}`"
        );
    }
}

#[test]
fn ui_is_split_into_markup_styles_and_logic() {
    let html = index_html();

    for asset in [
        "href=\"styles.css\"",
        "src=\"presentation-core.js\"",
        "src=\"player-core.js\"",
        "src=\"gallery-window-core.js\"",
        "src=\"window-lifecycle-core.js\"",
        "src=\"app-core.js\"",
        "src=\"settings.js\"",
        "src=\"library.js\"",
        "src=\"cloud.js\"",
        "src=\"review-player.js\"",
        "src=\"first-run.js\"",
        "type=\"module\" src=\"bootstrap.mjs\"",
    ] {
        assert!(html.contains(asset), "index.html must reference {asset}");
    }

    let bootstrap = read_ui_js("bootstrap.mjs");
    assert!(
        bootstrap.contains("import { PresentationCore } from \"./presentation.mjs\"")
            && bootstrap.contains("import { PlayerCore } from \"./player-core.mjs\"")
            && bootstrap.contains("import { CloudCore } from \"./cloud-core.mjs\"")
            && bootstrap
                .contains("import { GalleryWindowCore } from \"./gallery-window-core.mjs\"")
            && bootstrap
                .contains("import { WindowLifecycleCore } from \"./window-lifecycle-core.mjs\"",)
            && bootstrap.contains("globalThis.CliplineModules = Object.freeze(")
            && bootstrap.contains("await import(\"./main.js\")"),
        "renderer startup must enter through an explicit ES-module core/controller boundary"
    );

    let gallery_core = html.find("src=\"gallery-window-core.js\"").unwrap();
    let lifecycle_core = html.find("src=\"window-lifecycle-core.js\"").unwrap();
    let app_core = html.find("src=\"app-core.js\"").unwrap();
    assert!(
        gallery_core < app_core && lifecycle_core < app_core,
        "pure gallery/lifecycle cores must load before controller state consumes them"
    );

    let presentation = read_ui_js("presentation-core.js");
    let library = read_ui_js("library.js");
    let cloud = read_ui_js("cloud.js");
    let player = player_core_js();
    assert!(presentation.contains("const clipNameStem ="));
    assert!(!library.contains("function clipFileStem("));
    assert!(!cloud.contains("function clipNameStem("));
    assert!(!player.contains("const MONTHS ="));

    assert!(
        !html.contains("<style"),
        "styles belong in ui/styles.css, not inline in index.html"
    );

    for (i, chunk) in html.split("<script").skip(1).enumerate() {
        let tag_end = chunk.find('>').expect("script tag closes");
        assert!(
            chunk[..tag_end].contains("src="),
            "script tag #{i} must load an external file (logic belongs in ui/*.js modules)"
        );
        let body_end = chunk.find("</script>").expect("script element closes");
        assert!(
            chunk[tag_end + 1..body_end].trim().is_empty(),
            "script tag #{i} must not have an inline body"
        );
    }
}

#[test]
fn first_run_setup_covers_approved_defaults_and_save_flow() {
    let html = index_html();
    let css = styles_css();
    let wizard = read_ui_js("first-run.js");
    let settings = read_ui_js("settings.js");
    let main = read_ui_js("main.js");
    let app = app_rs();

    for required in [
        "id=\"first-run-setup\"",
        "id=\"first-run-basics\"",
        "id=\"first-run-capture\"",
        "id=\"first-run-games\"",
        "id=\"first-run-review\"",
        "id=\"first-run-hotkey\" value=\"F6\"",
        "id=\"rail-hotkey\" class=\"rail-hotkey\" title=\"Save Replay: F6\">F6</div>",
        "id=\"rail-save\" title=\"Save Replay (F6)\"",
        "id=\"first-run-media-dir\"",
        "id=\"first-run-quota\" type=\"number\" min=\"1\" max=\"1000\" value=\"10\"",
        "id=\"first-run-startup\" type=\"checkbox\" checked",
        "id=\"first-run-output-enabled\" type=\"checkbox\" checked",
        "id=\"first-run-output-device\"",
        "id=\"first-run-output-volume\"",
        "id=\"first-run-split-output\"",
        "id=\"first-run-mic-enabled\"",
        "id=\"first-run-mic-device\"",
        "id=\"first-run-mic-volume\"",
        "id=\"first-run-mic-mono\" type=\"checkbox\" checked",
        "id=\"first-run-test-mic\"",
        "id=\"first-run-capture-target\"",
        "id=\"first-run-pause-no-game\" type=\"checkbox\" checked",
        "id=\"first-run-replay\" type=\"range\" min=\"5\" max=\"120\" step=\"5\" value=\"30\"",
        "id=\"first-run-resolution\"",
        "<option value=\"1080p\" selected>1080p</option>",
        "id=\"first-run-quality\" type=\"range\" min=\"0\" max=\"3\" step=\"1\" value=\"1\"",
        "id=\"first-run-fps\" type=\"range\" min=\"0\" max=\"3\" step=\"1\" value=\"1\"",
        "id=\"first-run-supported-games\"",
        "id=\"first-run-detect-games\"",
        "id=\"first-run-detected-games\"",
        "id=\"first-run-finish\"",
        "id=\"first-run-error\"",
    ] {
        assert!(
            html.contains(required),
            "first-run setup must include `{required}`"
        );
    }

    assert!(
        css.contains(".first-run-setup[hidden]")
            && css.contains(".first-run-empty-state[hidden]")
            && css.contains("display: none"),
        "first-run overlay and dynamic states need explicit hidden rules"
    );
    assert!(
        css_rule_body(&css, ".first-run-folder-control input")
            .contains("width: 0 !important"),
        "the media-folder input must flex inside the row without pushing Browse out of bounds"
    );
    assert!(
        css_rule_body(&css, ".first-run-actions").contains("padding-right: 28px"),
        "wizard navigation should sit near the bottom-right window edge"
    );
    let detected_row_start = css
        .rfind(".first-run-detected-game {")
        .expect("detected-game override");
    assert!(
        css_rule_body(&css[detected_row_start..], ".first-run-detected-game {")
            .contains("grid-template-columns: auto auto minmax(0, 1fr)"),
        "detected games must align checkbox, icon, and text in that order"
    );
    for required in [
        "invoke(\"detect_installed_games\"",
        "invoke(\"save_settings\"",
        "invoke(\"set_recording\", { recording: true })",
        "customGameFromDetectedCandidate",
        "gamePluginSetting",
        "if (event.key === \"Tab\") return",
        "$(\"first-run-next\").focus()",
    ] {
        assert!(
            wizard.contains(required),
            "first-run controller must include `{required}`"
        );
    }
    assert!(
        main.contains("invoke(\"needs_first_run_setup\")")
            && main.contains("openFirstRunSetup(settings)"),
        "startup must ask the backend whether to open the setup wizard"
    );
    assert!(
        settings.contains("$(\"set-hotkey\").value || \"F6\"")
            && settings.contains("String(hotkey || \"F6\")")
            && app.contains("vec![parse_hotkey(\"F6\").unwrap()]"),
        "all startup UI and registration fallbacks must use the shipped F6 default"
    );
}

#[test]
fn first_run_setup_offers_a_one_click_recommended_preset() {
    let html = index_html();
    let wizard = read_ui_js("first-run.js");

    for required in [
        "id=\"first-run-auto-setup\"",
        ">Set this up for me</button>",
        "The default microphone will be enabled when one is available.",
        "id=\"first-run-auto-summary\"",
        "id=\"first-run-auto-summary-text\"",
        "id=\"first-run-auto-warning\"",
    ] {
        assert!(
            html.contains(required),
            "recommended setup UI must include `{required}`"
        );
    }

    let helper_start = wizard
        .find("async function applyFirstRunRecommendedSetup()")
        .expect("recommended setup helper");
    let helper_end = wizard[helper_start..]
        .find("\n}\n\nfunction updateFirstRunReview")
        .map(|offset| helper_start + offset)
        .expect("recommended setup helper end");
    let helper = &wizard[helper_start..helper_end];
    for required in [
        "$(\"first-run-hotkey\").value = \"F6\"",
        "$(\"first-run-quota\").value = \"10\"",
        "$(\"first-run-startup\").checked = true",
        "$(\"first-run-output-enabled\").checked = true",
        "$(\"first-run-split-output\").checked = false",
        "audioDevices.inputs.length > 0",
        "$(\"first-run-pause-no-game\").checked = true",
        "$(\"first-run-replay\").value = \"30\"",
        "$(\"first-run-resolution\").value = \"720p\"",
        "$(\"first-run-quality\").value = \"1\"",
        "$(\"first-run-fps\").value = \"1\"",
        "input.checked = true",
        "await detectFirstRunGames()",
        "firstRunSelectedCandidateIds.add(detectedGameKey(candidate))",
        "showFirstRunStep(3)",
    ] {
        assert!(
            helper.contains(required),
            "recommended preset must include `{required}`"
        );
    }
    assert!(
        !helper.contains("save_settings"),
        "recommended setup must remain a draft until Start Clipline"
    );
    assert!(
        wizard.contains("$(\"first-run-auto-setup\").addEventListener(\"click\"")
            && wizard.contains("firstRunRecommendation.warning")
            && wizard.contains("firstRunSelectedCandidateIds.size"),
        "recommended setup must be wired to Review with game and warning state"
    );
}

#[test]
fn first_run_setup_opens_on_a_full_app_welcome_choice() {
    let html = index_html();
    let css = styles_css();
    let wizard = read_ui_js("first-run.js");

    for required in [
        "id=\"first-run-intro\"",
        ">Welcome to Clipline.</h1>",
        "id=\"first-run-start-setup\"",
        ">Start setup</button>",
        "id=\"first-run-auto-setup\"",
        ">Set this up for me</button>",
    ] {
        assert!(
            html.contains(required),
            "first-run welcome screen must include `{required}`"
        );
    }
    let intro = html.find("id=\"first-run-intro\"").expect("welcome screen");
    let basics = html.find("id=\"first-run-basics\"").expect("Basics page");
    assert!(intro < basics, "welcome choices must precede the manual wizard");

    assert!(
        css.contains(".first-run-setup.intro")
            && css.contains(".first-run-intro")
            && css.contains(".first-run-intro[hidden]"),
        "the welcome screen must replace the normal wizard chrome"
    );
    for required in [
        "function showFirstRunIntro()",
        "overlay.classList.add(\"intro\")",
        "$(\"first-run-intro\").hidden = false",
        "overlay.classList.remove(\"intro\")",
        "$(\"first-run-intro\").hidden = true",
        "$(\"first-run-start-setup\").addEventListener(\"click\"",
        "showFirstRunIntro();",
    ] {
        assert!(
            wizard.contains(required),
            "welcome-screen controller must include `{required}`"
        );
    }
}

#[test]
fn first_run_detected_games_use_one_step_selection() {
    let html = index_html();
    let wizard = read_ui_js("first-run.js");

    assert!(
        html.contains("id=\"first-run-select-all\" type=\"checkbox\" disabled")
            && html.contains(">Select all</span>"),
        "detected games must offer a Select all checkbox"
    );
    let select_all = html.find("id=\"first-run-select-all\"").expect("Select all");
    let detect = html.find("id=\"first-run-detect-games\"").expect("Detect Games");
    let detected_list = html
        .find("id=\"first-run-detected-games\"")
        .expect("detected games list");
    assert!(
        select_all < detect && detect < detected_list,
        "Select all must sit beside Detect Games above the detected list"
    );
    assert!(
        !html.contains("id=\"first-run-add-games\"")
            && !wizard.contains("$(\"first-run-add-games\")"),
        "game selection must not require a separate Add selected games action"
    );
    for required in [
        "$(\"first-run-select-all\").addEventListener(\"change\"",
        "selectAll.disabled = firstRunCandidates.length === 0",
        "selectAll.indeterminate = count > 0 && count < firstRunCandidates.length",
        "if (firstRunStep === 2 && firstRunSelectedCandidateIds.size)",
        "addFirstRunDetectedGames();",
    ] {
        assert!(
            wizard.contains(required),
            "one-step detected-game selection must include `{required}`"
        );
    }
    let next_handler = wizard
        .find("$(\"first-run-next\").addEventListener(\"click\"")
        .expect("Continue handler");
    let next_handler = &wizard[next_handler..];
    assert!(
        next_handler.find("addFirstRunDetectedGames();")
            < next_handler.find("showFirstRunStep(firstRunStep + 1);"),
        "Continue must add selected games before rendering Review"
    );
}

#[test]
fn settings_misc_tab_replays_first_run_setup() {
    let html = index_html();
    let wizard = read_ui_js("first-run.js");

    for required in [
        "id=\"settings-tab-misc\"",
        "id=\"settings-panel-misc\"",
        "id=\"play-first-run-wizard\"",
        ">Play first-time wizard</button>",
    ] {
        assert!(
            html.contains(required),
            "settings wizard replay UI must include `{required}`"
        );
    }
    for required in [
        "$(\"play-first-run-wizard\").addEventListener(\"click\"",
        "settingsHaveUnsavedChanges()",
        "showSettingsDiscardWarning()",
        "toggleSettings(false)",
        "openFirstRunSetup(currentSettings)",
    ] {
        assert!(
            wizard.contains(required),
            "settings wizard replay action must include `{required}`"
        );
    }
    let open_start = wizard
        .find("async function openFirstRunSetup")
        .expect("first-run setup opener");
    let open_end = wizard[open_start..]
        .find("\n}\n\n$(\"play-first-run-wizard\")")
        .map(|offset| open_start + offset)
        .expect("first-run setup opener end");
    let opener = &wizard[open_start..open_end];
    for required in [
        "finish.disabled = false",
        "finish.textContent = \"Start Clipline\"",
        "$(\"first-run-back\").disabled = false",
    ] {
        assert!(
            opener.contains(required),
            "reopening the wizard must reset `{required}`"
        );
    }
}

#[test]
fn gallery_supports_multi_select_bulk_actions() {
    let html = index_html();
    let js = main_js();
    let css = styles_css();
    let library = library_rs();
    let app = app_rs();

    for required in [
        "id=\"gallery-select-toggle\"",
        ">Select multiple</button>",
        "class=\"gallery-filter-row\"",
        "class=\"gallery-filter-chips\"",
        "class=\"gallery-filter-actions\"",
        "id=\"gallery-bulk-bar\"",
        "id=\"bulk-count\"",
        "id=\"bulk-select-all\"",
        ">Select page</button>",
        "id=\"bulk-clear\"",
        "id=\"bulk-delete\"",
        "id=\"confirm-title\"",
    ] {
        assert!(
            html.contains(required),
            "gallery multi-select markup must include `{required}`"
        );
    }

    let filter_chips = html
        .find("class=\"gallery-filter-chips\"")
        .expect("gallery filter chip row exists");
    let select_toggle = html
        .find("id=\"gallery-select-toggle\"")
        .expect("gallery select toggle exists");
    let bulk_bar = html
        .find("id=\"gallery-bulk-bar\"")
        .expect("gallery bulk action bar exists");
    let gallery_grid = html
        .find("id=\"gallery-grid\"")
        .expect("gallery grid exists");
    assert!(
        filter_chips < select_toggle && select_toggle < bulk_bar && bulk_bar < gallery_grid,
        "bulk action bar must live inside the filter toolbar before the grid"
    );
    assert!(
        html.find("id=\"gallery-sort\"")
            .expect("gallery sort exists")
            < filter_chips,
        "Select multiple should live in the filter toolbar, not the main gallery header"
    );

    for required in [
        "selectedClipPaths",
        "selectMode",
        "function toggleClipSelection",
        "function clearSelection",
        "function selectAllVisible",
        "function exitSelectMode",
        "function syncSelectionControls",
        "function syncBulkBar",
        "function applyDeletion",
        "function deletionNotice",
        "function confirmBulkDelete",
        "function bulkDeleteSelected",
        "const DEFAULT_DELETE_CONFIRM_TITLE",
        "dataset.clipPath",
        "selectedClipPaths.has(c.path)",
        "Select multiple",
        "Done",
        "selectMode || count > 0",
        "await invoke(\"delete_clips\"",
        "gallerySource !== \"local\"",
    ] {
        assert!(
            js.contains(required),
            "main.js must wire multi-select behavior through `{required}`"
        );
    }

    assert!(
        library.contains("pub async fn delete_clips")
            && library.contains("fn delete_clips_impl")
            && library.contains("fn remove_clip_files")
            && library.contains("DeletedClipsReport"),
        "library.rs must expose a shared deletion helper, batch delete command, testable core, and report struct"
    );
    assert!(
        app.contains("crate::library::delete_clips"),
        "native command registry must register delete_clips"
    );

    for required in [
        ".gallery-bulk-bar",
        ".gallery-bulk-bar[hidden]",
        ".gallery-filter-row",
        ".gallery-filter-chips",
        ".gallery-filter-actions",
        ".gallery-grid.select-mode .card",
        ".gallery-grid.select-mode .card-del",
        ".card.selected",
    ] {
        assert!(
            css.contains(required),
            "multi-select UI needs stable styling for `{required}`"
        );
    }
    assert!(
        !js.contains("card-check")
            && !js.contains("check.addEventListener")
            && !js.contains("bulkUploadSelected")
            && !js.contains("uploadOneClipBulk")
            && !css.contains(".card-check"),
        "multi-select mode should use whole-card selection, not a competing per-card checkbox"
    );
    assert!(
        !html.contains("id=\"bulk-upload\"") && !html.contains("Upload to cloud"),
        "bulk actions should not expose bulk cloud upload"
    );
    assert!(
        !html.contains("id=\"bulk-cancel\"") && !js.contains("$(\"bulk-cancel\")"),
        "the active Select multiple control should be the single manual exit from select mode"
    );

    let delete_clip_fn = js
        .split("async function deleteClip")
        .nth(1)
        .and_then(|rest| rest.split("async function openFolder").next())
        .expect("deleteClip function body exists");
    assert!(
        delete_clip_fn.contains("await applyDeletion([path]);"),
        "single delete should use the shared post-delete reconciliation helper"
    );

    let bulk_delete_fn = js
        .split("async function bulkDeleteSelected")
        .nth(1)
        .and_then(|rest| rest.split("/* ---- backend events ---- */").next())
        .expect("bulkDeleteSelected function body exists");
    assert!(
        bulk_delete_fn.contains("await applyDeletion(report.deleted);"),
        "bulk delete should refresh storage and close the current review through the shared helper"
    );
    assert!(
        bulk_delete_fn.contains("deletionNotice(report.deleted.length)"),
        "bulk delete should suppress zero-delete notices and pluralize nonzero deletes"
    );
    assert!(
        bulk_delete_fn.contains("formatDeletionFailures(report.failed)"),
        "bulk delete must surface partial failures even when the current clip was removed"
    );
    assert!(
        bulk_delete_fn.contains("if (report.deleted.length > 0) exitSelectMode();"),
        "bulk delete should leave select mode after at least one clip is removed"
    );

    let select_all_fn = js
        .split("function selectAllVisible")
        .nth(1)
        .and_then(|rest| rest.split("function exitSelectMode").next())
        .expect("selectAllVisible function body exists");
    assert!(
        !select_all_fn.contains("galleryGroups(sortGalleryClips(filterGalleryClips(clipsCache)))"),
        "selectAllVisible should select from rendered card paths without re-running the gallery pipeline"
    );

    let select_toggle_handler = js
        .split("$(\"gallery-select-toggle\").addEventListener(\"click\"")
        .nth(1)
        .and_then(|rest| rest.split("$(\"bulk-select-all\")").next())
        .expect("gallery select toggle handler exists");
    assert!(
        !select_toggle_handler.contains("gallery-grid")
            && !select_toggle_handler.contains("classList.add(\"select-mode\")")
            && !select_toggle_handler.contains("classList.remove(\"select-mode\")"),
        "select-mode visual class ownership should live in the selection sync helpers"
    );

    let delete_clip_rs = library
        .split("pub fn delete_clip")
        .nth(1)
        .and_then(|rest| rest.split("pub struct DeletedClipsReport").next())
        .expect("delete_clip command body exists");
    assert!(
        delete_clip_rs.contains("remove_clip_files(&target)"),
        "single delete should call the same file-removal helper as bulk delete"
    );
    let delete_clips_impl_rs = library
        .split("fn delete_clips_impl")
        .nth(1)
        .and_then(|rest| rest.split("pub async fn delete_clips").next())
        .expect("delete_clips_impl body exists");
    assert!(
        delete_clips_impl_rs.contains("remove_clip_files(&target)"),
        "bulk delete should call the shared file-removal helper"
    );
}

#[test]
fn returning_to_no_preview_selection_clears_stale_audio_status() {
    let review = read_ui_js("review-player.js");
    let request = js_function_body(&review, "requestSelectedAudioPreview");
    // The no-preview branch sits between the reviewSelectionNeedsPreview guard and the
    // selectionKey == currentReviewAudioKey early-exit that follows it.
    let no_preview_block = request
        .split("if (!PlayerCore.reviewSelectionNeedsPreview(tracks, selected)) {")
        .nth(1)
        .and_then(|rest| {
            rest.split("if (selectionKey === currentReviewAudioKey)")
                .next()
        })
        .expect("no-preview branch must sit between the two guards in requestSelectedAudioPreview");
    let key_assign = no_preview_block
        .find("currentReviewAudioKey = selectionKey;")
        .expect("no-preview branch must assign currentReviewAudioKey before returning");
    let status_clear = no_preview_block
        .find("setDeckStatus(audioSelectionLabel(clip), { transient: true });")
        .expect("no-preview branch must call setDeckStatus(audioSelectionLabel(clip), { transient: true }) to clear any stale switching-audio-tracks status");
    assert!(
        key_assign < status_clear,
        "setDeckStatus must appear after currentReviewAudioKey is updated so the label reflects the new selection"
    );
}

#[test]
fn gallery_card_hover_keeps_hit_target_stable() {
    let css = styles_css();
    let card_rule = css_rule_body(&css, ".card");
    let hover_rule = css_rule_body(&css, ".card:hover");
    let play_rule = css_rule_body(&css, ".card-play");
    let play_hover_rule = css_rule_body(&css, ".card:hover .card-play");
    let delete_rule = css_rule_body(&css, ".card-del");
    let delete_hover_rule = css_rule_body(&css, ".card:hover .card-del");

    assert_eq!(
        css_decl_value(hover_rule, "transform"),
        None,
        "gallery card hover must not move the card hit target; moving it can make hover/click oscillate at card edges"
    );
    assert!(
        !css_decl_value(card_rule, "transition")
            .unwrap_or_default()
            .split(',')
            .any(|part| part.trim_start().starts_with("transform")),
        "gallery cards should not transition their own transform because hover feedback must keep the hit target stable"
    );
    assert_eq!(
        css_decl_value(play_rule, "pointer-events"),
        Some("none"),
        "the decorative play overlay must not take hover/click hit testing from the card"
    );
    assert!(
        !css_decl_value(play_rule, "transition")
            .unwrap_or_default()
            .split(',')
            .any(|part| part.trim_start().starts_with("transform"))
            && css_decl_value(play_hover_rule, "transform").is_none(),
        "the full-size play overlay should not transform on hover because it participates in thumbnail hit testing"
    );
    assert_eq!(
        css_decl_value(delete_rule, "pointer-events"),
        Some("none"),
        "the invisible delete button should not be hit-testable before the card hover makes it visible"
    );
    assert_eq!(
        css_decl_value(delete_hover_rule, "pointer-events"),
        Some("auto"),
        "the delete button should become clickable only while visible on card hover"
    );
}

#[test]
fn player_shortcuts_defer_to_any_open_dialog() {
    let main = read_ui_js("main.js");
    let handler = main
        .split("document.addEventListener(\"keydown\", (ev) => {")
        .nth(1)
        .and_then(|rest| {
            rest.split("if (ev.code === \"Escape\" && settingsOpen)")
                .next()
        })
        .expect("global keydown handler has a dialog guard");

    assert!(
        handler.contains("document.querySelector(\"dialog[open]\")"),
        "the global keydown guard must automatically cover every open dialog"
    );
    for dialog_id in [
        "confirm-dialog",
        "quit-dialog",
        "update-dialog",
        "elevation-dialog",
        "upload-dialog",
        "detected-games-dialog",
        "game-window-picker-dialog",
        "rename-file-dialog",
        "game-plugin-settings-dialog",
        "keys-dialog",
    ] {
        assert!(
            !handler.contains(dialog_id),
            "the dialog guard must not maintain a drifting special case for {dialog_id}"
        );
    }
}

#[test]
fn cloud_auth_refresh_preserves_the_unsaved_settings_draft() {
    let cloud = read_ui_js("cloud.js");
    let reload = js_function_body(&cloud, "reloadSettings");
    let connect = js_function_body(&cloud, "connectCloud");
    let disconnect = js_function_body(&cloud, "disconnectCloud");

    assert!(
        reload.contains("CloudCore.mergeBackendCloudSettings")
            && reload.contains("settingsDraft")
            && reload.contains("settingsIndicatorBaseline")
            && !reload.contains("fillSettings("),
        "cloud auth reload must patch backend-owned cloud state instead of repainting all settings"
    );
    for (name, body) in [("connect", connect), ("disconnect", disconnect)] {
        let snapshot = body
            .find("syncSettingsDraftFromForm({ resetDiscard: false })")
            .unwrap_or_else(|| panic!("{name} must snapshot the settings form before auth"));
        let invoke = body
            .find("await invoke(")
            .unwrap_or_else(|| panic!("{name} must invoke its backend command"));
        assert!(
            snapshot < invoke,
            "{name} must preserve edits before awaiting auth"
        );
    }
}

#[test]
fn cloud_byte_progress_does_not_unconditionally_rebuild_the_gallery() {
    let main = read_ui_js("main.js");
    let handler = main
        .split("listen(\"cloud-upload-progress\", (e) => {")
        .nth(1)
        .and_then(|rest| rest.split("\n});").next())
        .expect("cloud upload progress handler");

    assert!(
        handler.contains("const update = upsertCloudProgress(progress)")
            && handler.contains("if (update.renderRequired) renderClips()"),
        "gallery rebuilding must be conditional on meaningful upload-record transitions"
    );
    let condition = handler
        .find("if (update.renderRequired) renderClips()")
        .expect("conditional gallery render");
    let percentage = handler
        .find("received_size_bytes")
        .expect("live byte progress");
    assert!(
        percentage < condition,
        "the constant-size percentage update must remain live for byte-only progress"
    );
    assert_eq!(
        handler.matches("renderClips()").count(),
        1,
        "the progress handler must not keep an unconditional gallery rebuild"
    );
}

#[test]
fn support_reports_require_preview_before_private_submission() {
    let html = index_html();
    let support = read_ui_js("support.js");
    let core = read_ui_js("support-core.js");
    let css = styles_css();

    for required in [
        "data-tab=\"support\"",
        "id=\"support-description\"",
        "id=\"support-prepare\"",
        "id=\"support-preparing\"",
        "id=\"support-preview\"",
        "id=\"support-send\"",
        "id=\"support-save-copy\"",
        "id=\"support-discard\"",
        "id=\"support-cancel\"",
        "id=\"support-copy-id\"",
        "id=\"support-diagnostics-location\"",
        "private report expires and is deleted after 30 days",
        "Recordings, clips, media filenames, screenshots, raw settings.json",
    ] {
        assert!(
            html.contains(required),
            "Support UI must disclose and expose {required}"
        );
    }
    assert_eq!(
        css_decl_value(css_rule_body(&css, ".support-section"), "display"),
        Some("grid"),
        "Support content must be a real grid so its spacing applies"
    );
    for selector in [
        ".support-preparing[hidden]",
        ".support-preview[hidden]",
        ".support-progress[hidden]",
        ".support-success[hidden]",
    ] {
        assert_eq!(
            css_decl_value(css_rule_body(&css, selector), "display"),
            Some("none"),
            "{selector} must override the visible Support panel display rule"
        );
    }
    assert!(
        core.contains("globalThis.SupportCore")
            && core.contains("function transitionSupportPhase")
            && core.contains("function supportView"),
        "Support workflow visibility must come from a DOM-free phase model"
    );
    let prepare = support
        .find("invoke(\"prepare_bug_report\"")
        .expect("support preparation command");
    let submit = support
        .find("invoke(\"submit_bug_report\"")
        .expect("support submission command");
    assert!(
        prepare < submit && support.contains("Review the included file list"),
        "submission must be a distinct action after preparing and previewing the bundle"
    );
    assert!(
        support.contains("const description = $(\"support-description\").value;")
            && !support.contains("$(\"support-description\").value.trim()"),
        "the problem description must be transmitted exactly as entered"
    );
    assert!(
        support.contains("invoke(\"save_prepared_bug_report\"")
            && support.contains("invoke(\"discard_bug_report\"")
            && support.contains("invoke(\"cancel_bug_report\"")
            && support.contains("invoke(\"diagnostics_location\"")
            && support.contains("invoke(\"support_capabilities\""),
        "prepared reports must support offline save, discard, and upload cancellation"
    );
    assert!(
        support.contains("renderSupportState(")
            && support.contains("upload_available")
            && support.contains("support-description-count"),
        "Support UI must render phases centrally and disclose development upload availability"
    );
    assert!(
        main_js().contains("function syncSettingsFooterForTab")
            && main_js().contains("Save Other Changes"),
        "the Support tab must not present an irrelevant Save Settings action"
    );
}

#[test]
fn settings_tabs_and_support_phases_are_accessible() {
    let html = index_html();
    let main = main_js();
    let support = read_ui_js("support.js");

    assert!(
        html.contains(r#"id="settings-tabs" class="settings-tabs" role="tablist""#),
        "settings navigation must expose tablist semantics"
    );
    for (name, selected) in [
        ("general", "true"),
        ("capture", "false"),
        ("recording", "false"),
        ("games", "false"),
        ("storage", "false"),
        ("cloud", "false"),
        ("hotkeys", "false"),
        ("misc", "false"),
        ("support", "false"),
    ] {
        assert!(
            html.contains(&format!(
                r#"id="settings-tab-{name}" class="tab{}" role="tab" aria-selected="{selected}" aria-controls="settings-panel-{name}" data-tab="{name}""#,
                if name == "general" { " active" } else { "" }
            )),
            "settings tab {name} must identify and control its panel"
        );
        let panel_id = format!(r#"id="settings-panel-{name}""#);
        let panel_start = html.find(&panel_id).expect("settings panel id");
        let panel_tag_start = html[..panel_start]
            .rfind("<div")
            .expect("settings panel tag");
        let panel_tag_end = html[panel_start..]
            .find('>')
            .map(|offset| panel_start + offset)
            .expect("settings panel tag close");
        let panel_tag = &html[panel_tag_start..=panel_tag_end];
        assert_eq!(tag_attr(panel_tag, "role"), Some("tabpanel"));
        let labelled_by = format!("settings-tab-{name}");
        assert_eq!(
            tag_attr(panel_tag, "aria-labelledby"),
            Some(labelled_by.as_str())
        );
    }
    assert!(
        main.contains(r#"setAttribute("aria-selected", String(t === tab))"#)
            && main.contains(r#"setAttribute("tabindex", t === tab ? "0" : "-1")"#),
        "tab activation must synchronize keyboard and selected state"
    );
    assert!(
        main.contains(r#"$("settings-tabs").addEventListener("keydown""#)
            && main.contains("\"ArrowLeft\"")
            && main.contains("\"ArrowRight\"")
            && main.contains("\"Home\"")
            && main.contains("\"End\"")
            && main.contains("activateSettingsTab(nextTab, { focus: true })"),
        "roving settings tabs must support the ARIA keyboard navigation pattern"
    );
    assert!(
        support.contains("focusSupportPhase")
            && support.contains(r#"target.focus({ preventScroll: true })"#),
        "Support phase changes must move focus to the newly visible result"
    );
}

#[test]
fn frontend_failures_are_forwarded_to_bounded_native_diagnostics() {
    let core = read_ui_js("app-core.js");
    assert!(
        core.contains("window.addEventListener(\"error\"")
            && core.contains("window.addEventListener(\"unhandledrejection\"")
            && core.contains("invoke(\"log_frontend_event\""),
        "global JavaScript failures must be sent to the validated native diagnostic command"
    );
}
