use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use clipline_shell::diagnostics::{
    format_panic_record, max_local_bytes, DiagnosticsService, PanicRecord, RecordOutcome,
    DIAGNOSTIC_BARRIER_TIMEOUT, DIAGNOSTIC_GENERATIONS, DIAGNOSTIC_GENERATION_BYTES,
    DIAGNOSTIC_QUEUE_LINES, MAX_DIAGNOSTIC_RECORD_BYTES, MAX_PANIC_RECORD_BYTES,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "clipline-shell-{label}-{}-{sequence}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn production_bounds_are_the_existing_contract() {
    assert_eq!(DIAGNOSTIC_QUEUE_LINES, 2_048);
    assert_eq!(MAX_DIAGNOSTIC_RECORD_BYTES, 16 * 1024);
    assert_eq!(DIAGNOSTIC_GENERATION_BYTES, 4 * 1024 * 1024);
    assert_eq!(DIAGNOSTIC_GENERATIONS, 5);
    assert_eq!(max_local_bytes(), 20 * 1024 * 1024);
    assert_eq!(DIAGNOSTIC_BARRIER_TIMEOUT.as_secs(), 15);
}

#[test]
fn flush_is_an_explicit_barrier_and_records_receive_process_identity() {
    let directory = TestDirectory::new("flush-barrier");
    let logs = directory.path().join("logs");
    let mut service = DiagnosticsService::start(&logs, "session-test", 42).unwrap();
    assert_eq!(service.directory(), logs);
    assert_eq!(
        service
            .sink()
            .record(br#"{"level":"INFO","event":"before_flush"}"#),
        RecordOutcome::Enqueued
    );
    service.flush().unwrap();

    let log = std::fs::read_to_string(service.active_path()).unwrap();
    let value: serde_json::Value = serde_json::from_str(log.trim()).unwrap();
    assert_eq!(value["event"], "before_flush");
    assert_eq!(value["session_id"], "session-test");
    assert_eq!(value["pid"], 42);
    assert_eq!(value["severity"], "INFO");
    assert_eq!(value["outcome"], "observed");
    assert!(value["duration_ms"].is_null());
    assert_eq!(service.stats().dropped_lines, 0);
    assert_eq!(service.stats().write_errors, 0);
    let retained_sink = service.sink();
    service.shutdown().unwrap();
    assert_eq!(
        retained_sink.record(br#"{"event":"too_late"}"#),
        RecordOutcome::Dropped
    );
    assert_eq!(retained_sink.dropped_lines(), 1);
}

#[test]
fn event_buffers_bound_input_and_emit_one_valid_truncation_record() {
    let directory = TestDirectory::new("record-bound");
    let mut service =
        DiagnosticsService::start(directory.path().join("logs"), "bounded", 7).unwrap();
    let mut event = service.sink().event_buffer();
    let oversized = vec![b'x'; MAX_DIAGNOSTIC_RECORD_BYTES * 2];
    assert_eq!(event.write(&oversized).unwrap(), oversized.len());
    event.flush().unwrap();
    drop(event);
    service.flush().unwrap();

    let log = std::fs::read_to_string(service.active_path()).unwrap();
    let line = log.lines().next().unwrap();
    assert!(line.len() <= MAX_DIAGNOSTIC_RECORD_BYTES);
    let value: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(value["record_truncated"], true);
    service.shutdown().unwrap();
}

#[test]
fn record_producers_are_lossy_and_never_wait_for_disk() {
    let directory = TestDirectory::new("lossy");
    let mut service = DiagnosticsService::start(directory.path().join("logs"), "lossy", 9).unwrap();
    let sink = service.sink();
    let record = vec![b'x'; MAX_DIAGNOSTIC_RECORD_BYTES + 1];
    let mut observed_drop = false;
    for _ in 0..200_000 {
        if sink.record(&record) == RecordOutcome::Dropped {
            observed_drop = true;
            break;
        }
    }
    assert!(
        observed_drop,
        "a saturated producer must observe a lossy drop"
    );
    assert!(sink.dropped_lines() >= 1);
    service.shutdown().unwrap();
}

#[test]
fn snapshot_copies_only_allowlisted_files_at_the_barrier() {
    let source = TestDirectory::new("snapshot-source");
    let destination = TestDirectory::new("snapshot-destination");
    let logs = source.path().join("logs");
    let mut service = DiagnosticsService::start(&logs, "snapshot", 10).unwrap();
    service.sink().record(br#"{"event":"included"}"#);
    std::fs::write(logs.join("not-a-log.txt"), "private").unwrap();

    let copied = service.snapshot_to(destination.path()).unwrap();
    assert_eq!(copied, vec![destination.path().join("clipline.jsonl")]);
    let snapshot = std::fs::read_to_string(destination.path().join("clipline.jsonl")).unwrap();
    assert!(snapshot.contains("included"));
    assert!(!destination.path().join("not-a-log.txt").exists());
    service.shutdown().unwrap();
}

#[test]
fn panic_records_are_single_line_sanitized_utf8_safe_and_bounded() {
    let payload = format!("first line\n{}", "é".repeat(MAX_PANIC_RECORD_BYTES));
    let formatted = format_panic_record(PanicRecord {
        version: "1.2.3",
        pid: 11,
        thread_name: "test\nthread",
        location: "source.rs:12",
        payload: &payload,
        backtrace: &"frame\n".repeat(MAX_PANIC_RECORD_BYTES),
    });
    assert!(formatted.len() <= MAX_PANIC_RECORD_BYTES);
    assert!(formatted.ends_with("<panic record truncated>\n"));
    assert!(std::str::from_utf8(formatted.as_bytes()).is_ok());
    assert!(formatted.contains("payload=\"first line"));
    assert!(!formatted.contains("test\nthread"));
}

#[test]
fn panic_writer_appends_and_snapshot_includes_only_bounded_panic_files() {
    let source = TestDirectory::new("panic-source");
    let destination = TestDirectory::new("panic-destination");
    let mut service = DiagnosticsService::start(source.path().join("logs"), "panic", 12).unwrap();
    let written = service
        .panic_writer()
        .write(PanicRecord {
            version: "test",
            pid: 12,
            thread_name: "main",
            location: "test:1",
            payload: "panic payload",
            backtrace: "backtrace",
        })
        .unwrap();
    assert!(written);
    let copied = service.snapshot_to(destination.path()).unwrap();
    assert!(copied.contains(&destination.path().join("panic.log")));
    assert!(
        destination
            .path()
            .join("panic.log")
            .metadata()
            .unwrap()
            .len()
            <= u64::try_from(MAX_PANIC_RECORD_BYTES).unwrap()
    );
    service.shutdown().unwrap();
}

#[test]
fn writer_rotates_at_four_mib_and_retains_at_most_five_generations() {
    let directory = TestDirectory::new("rotation");
    let logs = directory.path().join("logs");
    let mut service = DiagnosticsService::start(&logs, "rotation", 13).unwrap();
    let padding = "x".repeat(15_000);
    for batch in 0..4 {
        for index in 0..100 {
            let record = serde_json::json!({
                "level": "INFO",
                "event": "rotation_record",
                "index": batch * 100 + index,
                "padding": padding,
            });
            assert_eq!(
                service.sink().record(&serde_json::to_vec(&record).unwrap()),
                RecordOutcome::Enqueued
            );
        }
        service.flush().unwrap();
    }
    assert!(logs.join("clipline.1.jsonl").is_file());
    for index in 0..DIAGNOSTIC_GENERATIONS {
        let path = if index == 0 {
            logs.join("clipline.jsonl")
        } else {
            logs.join(format!("clipline.{index}.jsonl"))
        };
        if path.exists() {
            assert!(path.metadata().unwrap().len() <= DIAGNOSTIC_GENERATION_BYTES);
        }
    }
    assert!(!logs.join("clipline.5.jsonl").exists());
    service.shutdown().unwrap();
}
