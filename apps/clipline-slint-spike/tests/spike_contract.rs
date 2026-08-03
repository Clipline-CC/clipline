use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const REQUIRED_SLINT_FEATURES: &[&str] = &[
    "accessibility",
    "backend-winit",
    "compat-1-2",
    "raw-window-handle-06",
    "renderer-software",
    "std",
    "system-tray",
];

const FORBIDDEN_DEPENDENCY_TOKENS: &[&str] = &[
    "backend-qt",
    "gstreamer",
    "renderer-skia",
    "tauri",
    "webview",
];

const REQUIRED_UI_CONTRACT: &[&str] = &[
    "in property <[LibraryItem]> library-items",
    "in property <[TimelineMarker]> timeline-markers",
    "in-out property <bool> review-visible",
    "in-out property <bool> cpu-frame-diagnostic",
    "in-out property <image> cpu-video-frame",
    "in-out property <string> presentation-path",
    "out property <length> video-stage-x",
    "out property <length> video-stage-y",
    "out property <length> video-stage-width",
    "out property <length> video-stage-height",
    "callback show-library()",
    "callback show-review()",
    "callback play-pause()",
    "callback seek(relative-seconds: float)",
    "callback set-track(track-id: int, selected: bool)",
    "callback set-volume(value: float)",
    "callback video-geometry-changed()",
    "source: root.cpu-video-frame",
    "D3D11 child window · fast path",
    "CPU diagnostic · SharedPixelBuffer",
    "accessible-label: \"Local Library\"",
    "accessible-label: \"Review player\"",
];

const REQUIRED_TELEMETRY_CONTRACT: &[&str] = &[
    "lateDropRatio",
    "avErrorP95Ms",
    "avErrorHistogramOverflowed",
    "seekSettleP95Ms",
    "seekLatencyHistogramOverflowed",
    "settledSeeks",
    "mftSamplesReceived",
    "mftSamplesReleased",
    "pendingHighWater",
    "swapChainBuffers",
    "decoderSurfacePool",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("spike package should be nested below workspace/apps")
        .to_path_buf()
}

fn read(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path.as_ref())
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.as_ref().display()))
}

fn rust_files_below(directory: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("failed to enumerate {}: {error}", directory.display()))
    {
        let path = entry.expect("read source entry").path();
        if path.is_dir() {
            files.extend(rust_files_below(&path));
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
    files
}

#[test]
fn spike_is_exactly_pinned_small_and_non_distributed() {
    let root = workspace_root();
    let manifest = read(root.join("apps/clipline-slint-spike/Cargo.toml"));
    let normalized = manifest.to_ascii_lowercase();

    let workspace = read(root.join("Cargo.toml"));
    assert!(
        workspace.contains("exclude = [\"third-party/shiguredo_opus\", \"apps/clipline-slint-spike\"]"),
        "the spike needs its own lockfile because Slint 1.17.1 and shipping Boa require incompatible ICU minor lines"
    );
    assert!(
        manifest.contains("[workspace]\nresolver = \"2\""),
        "the excluded spike must be an explicit standalone workspace"
    );
    assert!(
        manifest.contains("[profile.benchmark]")
            && manifest.contains("inherits = \"release\"")
            && manifest.contains("debug-assertions = true"),
        "formal Slint measurements need the optimized registry-safe benchmark profile"
    );

    assert!(
        manifest.contains("version = \"=1.17.1\""),
        "Slint and slint-build must be pinned to exactly 1.17.1"
    );
    assert!(
        manifest.contains("default-features = false"),
        "Slint default features must be disabled"
    );
    for feature in REQUIRED_SLINT_FEATURES {
        assert!(
            manifest.contains(&format!("\"{feature}\"")),
            "missing required Slint feature {feature}"
        );
    }
    for token in FORBIDDEN_DEPENDENCY_TOKENS {
        assert!(
            !normalized.contains(token),
            "spike manifest must not contain forbidden dependency/feature token {token}"
        );
    }
    assert!(manifest.contains("publish = false"));

    let build = read(root.join("apps/clipline-slint-spike/build.rs"));
    assert!(build.contains("slint_build::compile(\"ui/app.slint\")"));
    assert!(build.contains("CLIPLINE_BUILD_OPT_LEVEL"));
    let ui = read(root.join("apps/clipline-slint-spike/ui/app.slint"));
    assert!(ui.contains("export component CliplineSpike inherits Window"));
    assert!(ui.contains("preferred-width: 1200px"));
    assert!(ui.contains("preferred-height: 760px"));
    for contract in REQUIRED_UI_CONTRACT {
        assert!(
            ui.contains(contract),
            "missing Slint UI contract: {contract}"
        );
    }
    for tray_contract in [
        "clicked => { root.show-window(); }",
        "callback save-replay()",
        "callback open-diagnostics()",
        "title: \"Open Clipline\"",
        "title: root.save-replay-label",
        "title: \"Open Diagnostics Folder\"",
        "title: \"Quit\"",
    ] {
        assert!(
            ui.contains(tray_contract),
            "missing tray-first contract: {tray_contract}"
        );
    }
    assert!(
        !ui.contains("global "),
        "tray/window state must not use a Slint global"
    );

    let main = read(root.join("apps/clipline-slint-spike/src/main.rs"));
    assert!(main.contains("--clipline-benchmark-probe"));
    for contract in REQUIRED_TELEMETRY_CONTRACT {
        assert!(
            main.contains(contract),
            "missing final telemetry contract: {contract}"
        );
    }
    assert!(!main.contains("RuntimeState"));
    assert!(!main.contains("CloseRequestResponse::HideWindow"));
    assert!(main.contains("ShellLaunch::parse"));
    assert!(main.contains("run_windows_shell(options)"));

    let shell = read(root.join("apps/clipline-slint-spike/src/shell.rs"));
    for shell_contract in [
        "io.clipline.app.slint-spike",
        "acquire_or_activate(",
        "WindowsHotkeyService::start",
        "SlintDesktopAdapter::start_with_tray",
        "Option<WindowResources>",
        "slint::run_event_loop_until_quit()",
        "Rc::downgrade",
        "CloseRequestResponse::KeepWindowShown",
        "100 real Slint create/drop cycles",
        "pending_review_open: Option<DeferredReviewOpen>",
        "CatalogEffect::CancelCloudReviewMedia { owner }",
        "CatalogEffect::OpenPreparedCloudReview { owner, media }",
        "CatalogEffect::ReleaseCloudReviewMedia { lease_id }",
        "effects.filter(is_review_cleanup_effect)",
        "ValidatedLiveMediaSource::cached_cloud(lease)",
    ] {
        assert!(
            shell.contains(shell_contract),
            "missing lazy shell contract: {shell_contract}"
        );
    }
    assert!(!shell.contains("CloseRequestResponse::HideWindow"));
    assert!(shell.contains("stop_observed"));
    let shell_drop = shell
        .split("impl Drop for SlintShell")
        .nth(1)
        .and_then(|tail| tail.split("enum DeferredReviewOpen").next())
        .expect("SlintShell Drop body");
    let session_shutdown = shell_drop.find("session.shutdown()").unwrap();
    let cloud_shutdown = shell_drop.find("catalog_cloud.shutdown()").unwrap();
    assert!(
        session_shutdown < cloud_shutdown,
        "exceptional shell drop must join playback before Cloud cache/runtime shutdown"
    );
    let live = read(root.join("apps/clipline-slint-spike/src/live.rs"));
    assert!(
        !live.contains("stop_path"),
        "the stop watcher must outlive every window-owned playback session"
    );
    assert!(live.contains("impl Drop for LiveSession"));

    for config in [
        "apps/clipline-app/tauri.conf.json",
        "apps/clipline-app/tauri.standalone.conf.json",
    ] {
        assert!(
            !read(root.join(config)).contains("clipline-slint-spike"),
            "shipping bundle config must not reference the spike: {config}"
        );
    }

    for path in rust_files_below(&root.join("apps/clipline-slint-spike/src")) {
        assert!(
            !read(&path).contains("unsafe"),
            "spike Rust must call safe Windows wrappers: {}",
            path.display()
        );
    }
}

#[test]
fn package_probe_is_exact_side_effect_free_and_variant_intrinsic() {
    let output = Command::new(env!("CARGO_BIN_EXE_clipline-slint-spike"))
        .arg("--clipline-package-probe")
        .output()
        .expect("run side-effect-free package probe");
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert!(output.stdout.len() <= 4 * 1024);

    let probe: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("package probe should emit one JSON value");
    let object = probe.as_object().expect("package probe object");
    assert_eq!(object.len(), 9, "package probe field set must stay exact");
    assert_eq!(probe["schemaVersion"], 1);
    assert_eq!(probe["kind"], "clipline-slint-internal-candidate");
    assert_eq!(probe["productName"], "Clipline");
    assert_eq!(probe["publisher"], "Clipline");
    assert_eq!(probe["identifier"], "io.clipline.app");
    assert_eq!(probe["version"], "0.1.44");
    let expected_variant = if cfg!(feature = "package-regular") {
        "regular"
    } else if cfg!(feature = "package-standalone") {
        "standalone"
    } else {
        "unpackaged"
    };
    assert_eq!(probe["variant"], expected_variant);
    assert_eq!(probe["applicationStateStarted"], false);
    assert_eq!(probe["autostartRegistryMutation"], false);

    let rejected = Command::new(env!("CARGO_BIN_EXE_clipline-slint-spike"))
        .args(["--clipline-package-probe", "--autostart"])
        .output()
        .expect("run malformed package probe");
    assert!(!rejected.status.success());
    assert!(rejected.stdout.is_empty());
}

#[cfg(windows)]
#[test]
fn package_install_fence_matches_the_internal_nsis_candidate() {
    let shell = include_str!("../src/shell.rs");
    assert!(shell.contains(r"Local\io.clipline.app.slint-candidate.package-fence"));
    assert!(shell.contains("WindowsProcessFence::acquire"));
}
