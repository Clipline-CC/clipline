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
        "--scenario",
        "review-playing",
        "--marker-path",
        "ready.jsonl",
        "--stop-path",
        "driver.stop",
        "--telemetry-path",
        "telemetry.json",
    ]))
    .unwrap();
    assert_eq!(
        options.fixture.unwrap(),
        std::path::PathBuf::from("fixture.mp4")
    );
    assert_eq!(options.renderer, "winit-software");
    assert!(options.cpu_frame_diagnostic);
    assert!(options.exit_after_ready);
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
