use std::ffi::OsString;
use std::fs;

use clipline_slint_spike::options::{write_marker, OptionsError, SpikeOptions, SpikeScenario};

fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

#[test]
fn command_line_selects_only_explicit_bounded_paths() {
    let options = SpikeOptions::parse(args(&[
        "spike",
        "--fixture",
        "fixture.mp4",
        "--renderer",
        "winit-software",
        "--cpu-frame-diagnostic",
        "--exit-after-ready",
        "--autostart",
        "--scenario",
        "review-playing",
        "--marker-path",
        "ready.jsonl",
        "--stop-path",
        "driver.stop",
        "--telemetry-path",
        "telemetry.json",
        "--settings-profile",
        "isolated-profile",
    ]))
    .unwrap();
    assert_eq!(
        options.fixture.unwrap(),
        std::path::PathBuf::from("fixture.mp4")
    );
    assert_eq!(options.renderer, "winit-software");
    assert!(options.cpu_frame_diagnostic);
    assert!(options.exit_after_ready);
    assert!(options.autostart);
    assert_eq!(options.scenario, SpikeScenario::ReviewPlaying);
    assert_eq!(
        options.marker_path.unwrap(),
        std::path::PathBuf::from("ready.jsonl")
    );
    assert_eq!(
        options.stop_path.unwrap(),
        std::path::PathBuf::from("driver.stop")
    );
    assert_eq!(
        options.telemetry_path.unwrap(),
        std::path::PathBuf::from("telemetry.json")
    );
    assert_eq!(
        options.settings_profile.unwrap(),
        std::path::PathBuf::from("isolated-profile")
    );

    assert!(matches!(
        SpikeOptions::parse(args(&["spike", "--renderer", "winit-femtovg"])),
        Err(OptionsError::UnsupportedRenderer(_))
    ));
    assert!(matches!(
        SpikeOptions::parse(args(&["spike", "--scenario", "scrub-storm"])),
        Err(OptionsError::FixtureRequired(SpikeScenario::ScrubStorm))
    ));
}

#[test]
fn autostart_is_opt_in_and_fixtureless_for_the_interactive_tray_shell() {
    let defaults = SpikeOptions::parse(args(&["spike"])).unwrap();
    assert!(!defaults.autostart);

    let autostart = SpikeOptions::parse(args(&["spike", "--autostart"])).unwrap();
    assert!(autostart.autostart);
    assert_eq!(autostart.scenario, SpikeScenario::Interactive);
    assert!(autostart.fixture.is_none());
    assert!(SpikeOptions::usage().contains("--autostart"));
}

#[test]
fn autostart_does_not_relax_fixture_requirements_for_media_scenarios() {
    assert!(matches!(
        SpikeOptions::parse(args(&[
            "spike",
            "--autostart",
            "--scenario",
            "reveal-close-100",
        ])),
        Err(OptionsError::FixtureRequired(SpikeScenario::RevealClose100))
    ));
}

#[test]
fn marker_is_append_only_frontend_neutral_json_lines() {
    let directory =
        std::env::temp_dir().join(format!("clipline-slint-marker-{}", std::process::id()));
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join("markers.jsonl");
    let _ = fs::remove_file(&path);
    write_marker(&path, "ready", "review-playing advancing").unwrap();
    write_marker(&path, "error", "synthetic").unwrap();
    let lines: Vec<_> = fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(lines.len(), 2);
    let ready: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(ready["schemaVersion"], 1);
    assert_eq!(ready["kind"], "ready");
    assert_eq!(ready["detail"], "review-playing advancing");
    assert!(ready["timestampUtc"].as_str().unwrap().ends_with('Z'));
    fs::remove_file(path).unwrap();
    fs::remove_dir(directory).unwrap();
}
