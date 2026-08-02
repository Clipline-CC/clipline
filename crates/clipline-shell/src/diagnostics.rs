//! Framework-neutral, bounded diagnostic log ownership.
//!
//! Frontend adapters may feed already-structured JSON records into a [`DiagnosticsSink`]. The
//! sink never blocks: a full or stopped writer drops the record and increments the dropped-line
//! counter. Snapshot, flush, and shutdown are explicit barriers with a fixed timeout.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, TryLockError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use thiserror::Error;

pub const DIAGNOSTIC_GENERATION_BYTES: u64 = 4 * 1024 * 1024;
pub const DIAGNOSTIC_GENERATIONS: usize = 5;
pub const MAX_DIAGNOSTIC_RECORD_BYTES: usize = 16 * 1024;
pub const DIAGNOSTIC_QUEUE_LINES: usize = 2_048;
pub const DIAGNOSTIC_BARRIER_TIMEOUT: Duration = Duration::from_secs(15);
pub const MAX_PANIC_FILE_BYTES: u64 = 512 * 1024;
pub const MAX_PANIC_RECORD_BYTES: usize = 128 * 1024;

const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const CONTROL_RETRY: Duration = Duration::from_millis(1);
const MAX_SESSION_ID_BYTES: usize = 256;

#[must_use]
pub const fn max_local_bytes() -> u64 {
    DIAGNOSTIC_GENERATION_BYTES * DIAGNOSTIC_GENERATIONS as u64
}

#[derive(Debug, Error)]
pub enum DiagnosticsError {
    #[error("invalid diagnostics configuration: {0}")]
    InvalidConfiguration(&'static str),
    #[error("diagnostic I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("diagnostic writer stopped before {0}")]
    WriterStopped(&'static str),
    #[error("timed out waiting for diagnostic {0} barrier")]
    BarrierTimeout(&'static str),
    #[error("diagnostic writer thread panicked")]
    WriterPanicked,
    #[error("diagnostics were already shut down")]
    AlreadyShutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordOutcome {
    Enqueued,
    Dropped,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DiagnosticsStats {
    pub dropped_lines: usize,
    pub write_errors: usize,
}

/// Nonblocking producer handle. Clones do not retain any frontend object.
#[derive(Clone)]
pub struct DiagnosticsSink {
    sender: SyncSender<WriterCommand>,
    dropped: Arc<AtomicUsize>,
    accepting: Arc<AtomicBool>,
}

/// An event buffer suitable for adapters which receive a diagnostic through `std::io::Write`.
pub struct DiagnosticEventBuffer {
    sink: DiagnosticsSink,
    bytes: Vec<u8>,
    sent: bool,
}

/// Process-local diagnostic service and its ordered writer thread.
pub struct DiagnosticsService {
    sender: Option<SyncSender<WriterCommand>>,
    sink: DiagnosticsSink,
    worker: Option<JoinHandle<()>>,
    write_errors: Arc<AtomicUsize>,
    directory: PathBuf,
    active_path: PathBuf,
    stopped: Arc<AtomicBool>,
    panic_writer: PanicWriter,
}

/// Cloneable, recursion-safe panic-log writer for use from a process panic hook.
#[derive(Clone)]
pub struct PanicWriter {
    directory: PathBuf,
    lock: Arc<Mutex<()>>,
}

#[derive(Clone, Copy, Debug)]
pub struct PanicRecord<'a> {
    pub version: &'a str,
    pub pid: u32,
    pub thread_name: &'a str,
    pub location: &'a str,
    pub payload: &'a str,
    pub backtrace: &'a str,
}

enum WriterCommand {
    Record(Vec<u8>),
    Snapshot {
        destination: PathBuf,
        result: mpsc::Sender<Result<Vec<PathBuf>, String>>,
    },
    Flush {
        result: mpsc::Sender<Result<(), String>>,
    },
    Shutdown {
        result: mpsc::Sender<Result<(), String>>,
    },
}

struct RollingFileWriter {
    directory: PathBuf,
    active_path: PathBuf,
    file: File,
    bytes_written: u64,
}

impl DiagnosticsService {
    /// Start the fixed production diagnostics service in `directory`.
    pub fn start(
        directory: impl Into<PathBuf>,
        session_id: impl Into<String>,
        pid: u32,
    ) -> Result<Self, DiagnosticsError> {
        let directory = directory.into();
        let session_id = session_id.into();
        if session_id.is_empty() || session_id.len() > MAX_SESSION_ID_BYTES {
            return Err(DiagnosticsError::InvalidConfiguration(
                "session id must contain 1..=256 UTF-8 bytes",
            ));
        }

        let rolling = RollingFileWriter::open(directory.clone())?;
        let active_path = rolling.active_path.clone();
        let (sender, receiver) = mpsc::sync_channel(DIAGNOSTIC_QUEUE_LINES);
        let dropped = Arc::new(AtomicUsize::new(0));
        let write_errors = Arc::new(AtomicUsize::new(0));
        let stopped = Arc::new(AtomicBool::new(false));
        let accepting = Arc::new(AtomicBool::new(true));
        let worker_write_errors = Arc::clone(&write_errors);
        let worker_stopped = Arc::clone(&stopped);
        let worker = std::thread::Builder::new()
            .name("clipline-diagnostics".into())
            .spawn(move || {
                writer_thread(receiver, rolling, session_id, pid, &worker_write_errors);
                worker_stopped.store(true, Ordering::Release);
            })?;
        let sink = DiagnosticsSink {
            sender: sender.clone(),
            dropped,
            accepting,
        };
        Ok(Self {
            sender: Some(sender),
            sink,
            worker: Some(worker),
            write_errors,
            directory: directory.clone(),
            active_path,
            stopped,
            panic_writer: PanicWriter {
                directory,
                lock: Arc::new(Mutex::new(())),
            },
        })
    }

    #[must_use]
    pub fn sink(&self) -> DiagnosticsSink {
        self.sink.clone()
    }

    #[must_use]
    pub fn panic_writer(&self) -> PanicWriter {
        self.panic_writer.clone()
    }

    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    #[must_use]
    pub fn active_path(&self) -> &Path {
        &self.active_path
    }

    #[must_use]
    pub fn stats(&self) -> DiagnosticsStats {
        DiagnosticsStats {
            dropped_lines: self.sink.dropped.load(Ordering::Relaxed),
            write_errors: self.write_errors.load(Ordering::Relaxed),
        }
    }

    /// Flush every record ordered before this command and wait for explicit acknowledgement.
    pub fn flush(&self) -> Result<(), DiagnosticsError> {
        let (result_tx, result_rx) = mpsc::channel();
        self.enqueue_control(WriterCommand::Flush { result: result_tx }, "flush")?;
        receive_barrier(result_rx, "flush")
    }

    /// Flush and copy only the allowlisted diagnostic files at this queue barrier.
    pub fn snapshot_to(
        &self,
        destination: impl Into<PathBuf>,
    ) -> Result<Vec<PathBuf>, DiagnosticsError> {
        let (result_tx, result_rx) = mpsc::channel();
        self.enqueue_control(
            WriterCommand::Snapshot {
                destination: destination.into(),
                result: result_tx,
            },
            "snapshot",
        )?;
        receive_barrier(result_rx, "snapshot")
    }

    /// Flush, receive a shutdown acknowledgement, and join the writer thread.
    pub fn shutdown(&mut self) -> Result<(), DiagnosticsError> {
        if self.sender.is_none() {
            return Err(DiagnosticsError::AlreadyShutdown);
        }
        self.sink.accepting.store(false, Ordering::Release);
        let (result_tx, result_rx) = mpsc::channel();
        self.enqueue_control(WriterCommand::Shutdown { result: result_tx }, "shutdown")?;
        receive_barrier(result_rx, "shutdown")?;
        self.sender.take();
        self.join_worker()
    }

    fn enqueue_control(
        &self,
        mut command: WriterCommand,
        operation: &'static str,
    ) -> Result<(), DiagnosticsError> {
        let sender = self
            .sender
            .as_ref()
            .ok_or(DiagnosticsError::AlreadyShutdown)?;
        let deadline = Instant::now() + DIAGNOSTIC_BARRIER_TIMEOUT;
        loop {
            match sender.try_send(command) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Disconnected(_)) => {
                    return Err(DiagnosticsError::WriterStopped(operation));
                }
                Err(TrySendError::Full(returned)) => {
                    command = returned;
                    if Instant::now() >= deadline {
                        return Err(DiagnosticsError::BarrierTimeout(operation));
                    }
                    std::thread::sleep(CONTROL_RETRY);
                }
            }
        }
    }

    fn join_worker(&mut self) -> Result<(), DiagnosticsError> {
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| DiagnosticsError::WriterPanicked)?;
        }
        Ok(())
    }
}

impl Drop for DiagnosticsService {
    fn drop(&mut self) {
        if self.sender.is_none() || self.stopped.load(Ordering::Acquire) {
            let _ = self.join_worker();
            return;
        }
        // Preserve the old guard's flush-and-join behavior while applying the same bounded
        // acknowledgement used by explicit lifecycle shutdown. Errors cannot be reported from
        // Drop; callers that need the acknowledgement must call `shutdown` themselves.
        let _ = self.shutdown();
    }
}

impl DiagnosticsSink {
    /// Enqueue one bounded record without waiting for disk I/O.
    pub fn record(&self, record: &[u8]) -> RecordOutcome {
        if !self.accepting.load(Ordering::Acquire) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return RecordOutcome::Dropped;
        }
        let mut bounded = Vec::with_capacity(record.len().min(MAX_DIAGNOSTIC_RECORD_BYTES + 1));
        bounded.extend_from_slice(&record[..record.len().min(MAX_DIAGNOSTIC_RECORD_BYTES + 1)]);
        match self.sender.try_send(WriterCommand::Record(bounded)) {
            Ok(()) => RecordOutcome::Enqueued,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                RecordOutcome::Dropped
            }
        }
    }

    #[must_use]
    pub fn event_buffer(&self) -> DiagnosticEventBuffer {
        DiagnosticEventBuffer {
            sink: self.clone(),
            bytes: Vec::with_capacity(1024),
            sent: false,
        }
    }

    #[must_use]
    pub fn dropped_lines(&self) -> usize {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl Write for DiagnosticEventBuffer {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let remaining = (MAX_DIAGNOSTIC_RECORD_BYTES + 1).saturating_sub(self.bytes.len());
        self.bytes
            .extend_from_slice(&buffer[..buffer.len().min(remaining)]);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.send();
        Ok(())
    }
}

impl DiagnosticEventBuffer {
    fn send(&mut self) {
        if !self.sent && !self.bytes.is_empty() {
            self.sent = true;
            let _ = self.sink.record(&std::mem::take(&mut self.bytes));
        }
    }
}

impl Drop for DiagnosticEventBuffer {
    fn drop(&mut self) {
        self.send();
    }
}

impl PanicWriter {
    /// Try to append one bounded panic record. Recursive/concurrent panic writers fail closed.
    pub fn write(&self, record: PanicRecord<'_>) -> Result<bool, DiagnosticsError> {
        let _guard = match self.lock.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::WouldBlock) => return Ok(false),
            Err(TryLockError::Poisoned(_)) => return Ok(false),
        };
        std::fs::create_dir_all(&self.directory)?;
        let path = self.directory.join("panic.log");
        let formatted = format_panic_record(record);
        let rotate = std::fs::metadata(&path)
            .is_ok_and(|metadata| panic_record_requires_rotation(metadata.len(), formatted.len()));
        if rotate {
            let old = self.directory.join("panic.old.log");
            match std::fs::remove_file(&old) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            std::fs::rename(&path, old)?;
        }
        let mut file = open_append(&path)?;
        file.write_all(formatted.as_bytes())?;
        file.flush()?;
        Ok(true)
    }
}

/// Format and UTF-8-bound a panic record without retaining the panic payload.
#[must_use]
pub fn format_panic_record(record: PanicRecord<'_>) -> String {
    let version = single_line_bounded(record.version, 256);
    let thread_name = single_line_bounded(record.thread_name, 256);
    let location = single_line_bounded(record.location, 1_024);
    let payload = single_line_bounded(record.payload, 16 * 1024);
    let backtrace = prefix_at_char_boundary(record.backtrace, MAX_PANIC_RECORD_BYTES);
    let mut formatted = format!(
        "{} version={} pid={} thread={:?} location={:?} payload={:?}\n{}\n",
        unix_timestamp_millis(),
        version,
        record.pid,
        thread_name,
        location,
        payload,
        backtrace,
    );
    if formatted.len() > MAX_PANIC_RECORD_BYTES {
        formatted.truncate(floor_char_boundary(&formatted, MAX_PANIC_RECORD_BYTES - 32));
        formatted.push_str("\n<panic record truncated>\n");
    }
    formatted
}

fn writer_thread(
    receiver: Receiver<WriterCommand>,
    mut rolling: RollingFileWriter,
    session_id: String,
    pid: u32,
    write_errors: &AtomicUsize,
) {
    while let Ok(command) = receiver.recv() {
        match command {
            WriterCommand::Record(buffer) => {
                let record = structured_record(&buffer, &session_id, pid);
                if rolling.write_record(&record).is_err() {
                    write_errors.fetch_add(1, Ordering::Relaxed);
                }
            }
            WriterCommand::Snapshot {
                destination,
                result,
            } => {
                let snapshot = rolling.snapshot(&destination);
                if snapshot.is_err() {
                    write_errors.fetch_add(1, Ordering::Relaxed);
                }
                let _ = result.send(snapshot.map_err(|error| error.to_string()));
            }
            WriterCommand::Flush { result } => {
                let flushed = rolling.file.flush();
                if flushed.is_err() {
                    write_errors.fetch_add(1, Ordering::Relaxed);
                }
                let _ = result.send(flushed.map_err(|error| error.to_string()));
            }
            WriterCommand::Shutdown { result } => {
                let flushed = rolling.file.flush();
                if flushed.is_err() {
                    write_errors.fetch_add(1, Ordering::Relaxed);
                }
                let _ = result.send(flushed.map_err(|error| error.to_string()));
                break;
            }
        }
    }
}

fn receive_barrier<T>(
    receiver: mpsc::Receiver<Result<T, String>>,
    operation: &'static str,
) -> Result<T, DiagnosticsError> {
    match receiver.recv_timeout(DIAGNOSTIC_BARRIER_TIMEOUT) {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(message)) => Err(DiagnosticsError::Io(io::Error::other(message))),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(DiagnosticsError::BarrierTimeout(operation)),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err(DiagnosticsError::WriterStopped(operation))
        }
    }
}

impl RollingFileWriter {
    fn open(directory: PathBuf) -> io::Result<Self> {
        std::fs::create_dir_all(&directory)?;
        prune_old_files(&directory, SystemTime::now(), MAX_AGE)?;
        let active_path = generation_path(&directory, 0);
        if std::fs::metadata(&active_path).is_ok_and(|metadata| {
            metadata.len() >= DIAGNOSTIC_GENERATION_BYTES
                || metadata
                    .modified()
                    .ok()
                    .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                    .is_some_and(|age| age > MAX_AGE)
        }) {
            rotate_generations(&directory)?;
        }
        let file = open_append(&active_path)?;
        let bytes_written = file.metadata()?.len();
        Ok(Self {
            directory,
            active_path,
            file,
            bytes_written,
        })
    }

    fn write_record(&mut self, record: &[u8]) -> io::Result<()> {
        let record = bound_record(record);
        let record_bytes = u64::try_from(record.len() + 1).unwrap_or(u64::MAX);
        if self.bytes_written.saturating_add(record_bytes) > DIAGNOSTIC_GENERATION_BYTES {
            self.rotate()?;
        }
        self.file.write_all(&record)?;
        self.file.write_all(b"\n")?;
        self.bytes_written = self.bytes_written.saturating_add(record_bytes);
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file.flush()?;
        rotate_generations(&self.directory)?;
        self.file = open_append(&self.active_path)?;
        self.bytes_written = self.file.metadata()?.len();
        Ok(())
    }

    fn snapshot(&mut self, destination: &Path) -> io::Result<Vec<PathBuf>> {
        self.file.flush()?;
        std::fs::create_dir_all(destination)?;
        let mut copied = Vec::new();
        for source in diagnostic_files(&self.directory) {
            if !source.is_file() {
                continue;
            }
            let Some(name) = source.file_name() else {
                continue;
            };
            let target = destination.join(name);
            std::fs::copy(&source, &target)?;
            copied.push(target);
        }
        if let Some(parent) = self.directory.parent() {
            for name in ["clipline.log", "clipline.old.log"] {
                let source = parent.join(name);
                if !legacy_log_is_recent(&source) {
                    continue;
                }
                let target = destination.join(name);
                std::fs::copy(&source, &target)?;
                copied.push(target);
            }
        }
        Ok(copied)
    }
}

fn structured_record(buffer: &[u8], session_id: &str, pid: u32) -> Vec<u8> {
    let text = String::from_utf8_lossy(buffer);
    let mut value = serde_json::from_str::<Value>(text.trim()).unwrap_or_else(|_| {
        json!({
            "timestamp_unix_ms": unix_timestamp_millis(),
            "level": "WARN",
            "target": "clipline_shell::diagnostics",
            "event": "unparseable_diagnostic",
            "message": single_line(&text),
        })
    });
    if let Some(object) = value.as_object_mut() {
        object.insert("session_id".into(), Value::String(session_id.to_owned()));
        object.insert("pid".into(), Value::Number(pid.into()));
        object
            .entry("event")
            .or_insert_with(|| Value::String("diagnostic".into()));
        let severity = object
            .get("level")
            .cloned()
            .unwrap_or_else(|| Value::String("WARN".into()));
        object.entry("severity").or_insert(severity);
        object
            .entry("outcome")
            .or_insert_with(|| Value::String("observed".into()));
        object.entry("duration_ms").or_insert(Value::Null);
    }
    let mut record = serde_json::to_vec(&value).unwrap_or_else(|_| {
        br#"{"level":"ERROR","event":"diagnostic_serialization_failed"}"#.to_vec()
    });
    if record.len() > MAX_DIAGNOSTIC_RECORD_BYTES {
        if let Some(object) = value.as_object_mut() {
            object.remove("stack");
            object.remove("spans");
            object.insert(
                "message".into(),
                Value::String("<diagnostic record truncated>".into()),
            );
            object.insert("record_truncated".into(), Value::Bool(true));
        }
        record = serde_json::to_vec(&value).unwrap_or_else(|_| {
            br#"{"level":"ERROR","event":"diagnostic_serialization_failed"}"#.to_vec()
        });
    }
    bound_record(&record)
}

fn bound_record(record: &[u8]) -> Vec<u8> {
    if record.len() <= MAX_DIAGNOSTIC_RECORD_BYTES {
        return record.to_vec();
    }
    br#"{"level":"WARN","event":"diagnostic_record_too_large","record_truncated":true}"#.to_vec()
}

fn single_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn single_line_bounded(value: &str, max_bytes: usize) -> String {
    let mut result = String::with_capacity(value.len().min(max_bytes));
    let mut needs_space = false;
    for character in value.chars() {
        if character.is_whitespace() {
            needs_space = !result.is_empty();
            continue;
        }
        let separator_bytes = usize::from(needs_space);
        if result
            .len()
            .saturating_add(separator_bytes)
            .saturating_add(character.len_utf8())
            > max_bytes
        {
            break;
        }
        if needs_space {
            result.push(' ');
            needs_space = false;
        }
        result.push(character);
    }
    result
}

fn prefix_at_char_boundary(value: &str, max_bytes: usize) -> &str {
    &value[..floor_char_boundary(value, max_bytes)]
}

fn diagnostic_files(directory: &Path) -> Vec<PathBuf> {
    std::iter::once(generation_path(directory, 0))
        .chain((1..DIAGNOSTIC_GENERATIONS).map(|index| generation_path(directory, index)))
        .chain(std::iter::once(directory.join("panic.log")))
        .chain(std::iter::once(directory.join("panic.old.log")))
        .collect()
}

fn generation_path(directory: &Path, index: usize) -> PathBuf {
    if index == 0 {
        directory.join("clipline.jsonl")
    } else {
        directory.join(format!("clipline.{index}.jsonl"))
    }
}

fn rotate_generations(directory: &Path) -> io::Result<()> {
    let oldest = generation_path(directory, DIAGNOSTIC_GENERATIONS - 1);
    match std::fs::remove_file(&oldest) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    for index in (1..DIAGNOSTIC_GENERATIONS).rev() {
        let source = generation_path(directory, index - 1);
        let target = generation_path(directory, index);
        match std::fs::rename(source, target) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn prune_old_files(directory: &Path, now: SystemTime, max_age: Duration) -> io::Result<()> {
    for path in diagnostic_files(directory) {
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        let is_old = metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > max_age);
        if is_old {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn legacy_log_is_recent(path: &Path) -> bool {
    path.metadata()
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age <= MAX_AGE)
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn panic_record_requires_rotation(current_bytes: u64, record_bytes: usize) -> bool {
    current_bytes.saturating_add(u64::try_from(record_bytes).unwrap_or(u64::MAX))
        > MAX_PANIC_FILE_BYTES
}

fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index > 0 && !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn unix_timestamp_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
