#![cfg(windows)]

use clipline_recorder::{active_recorder_workers, PreparedRecorderRestart, ServiceOptions};

#[test]
fn invalid_static_options_fail_before_a_worker_is_spawned() {
    let before = active_recorder_workers();
    let options = ServiceOptions {
        fps: 0,
        ..ServiceOptions::default()
    };

    let error = PreparedRecorderRestart::prepare(options)
        .err()
        .expect("zero frame rate must fail in preparation");

    assert!(error.contains("frame rate"));
    assert_eq!(active_recorder_workers(), before);
}

#[test]
fn dropping_a_valid_prepared_restart_joins_its_parked_worker() {
    let before = active_recorder_workers();
    let prepared = PreparedRecorderRestart::prepare(ServiceOptions::default()).unwrap();
    assert_eq!(active_recorder_workers(), before + 1);

    drop(prepared);

    assert_eq!(active_recorder_workers(), before);
}
