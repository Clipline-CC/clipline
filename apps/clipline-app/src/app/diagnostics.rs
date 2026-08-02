//! Tauri tracing adapter for the framework-neutral diagnostics service.

use std::backtrace::Backtrace;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once, OnceLock};

use clipline_shell::diagnostics::{
    DiagnosticEventBuffer, DiagnosticsService, DiagnosticsSink, PanicRecord, PanicWriter,
    DIAGNOSTIC_GENERATIONS, DIAGNOSTIC_GENERATION_BYTES, DIAGNOSTIC_QUEUE_LINES,
};
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::prelude::*;
use uuid::Uuid;

static DIAGNOSTICS: OnceLock<DiagnosticsHandle> = OnceLock::new();
static PANIC_HOOK: Once = Once::new();

pub(super) const fn max_local_bytes() -> u64 {
    clipline_shell::diagnostics::max_local_bytes()
}

pub(super) struct DiagnosticsGuard {
    service: Arc<Mutex<Option<DiagnosticsService>>>,
}

#[derive(Clone)]
struct DiagnosticsHandle {
    service: Arc<Mutex<Option<DiagnosticsService>>>,
    directory: PathBuf,
    active_path: PathBuf,
}

#[derive(Clone)]
struct DiagnosticMakeWriter {
    sink: DiagnosticsSink,
}

impl<'writer> MakeWriter<'writer> for DiagnosticMakeWriter {
    type Writer = DiagnosticEventBuffer;

    fn make_writer(&'writer self) -> Self::Writer {
        self.sink.event_buffer()
    }
}

impl Drop for DiagnosticsGuard {
    fn drop(&mut self) {
        let service = self
            .service
            .lock()
            .ok()
            .and_then(|mut service| service.take());
        if let Some(mut service) = service {
            let _ = service.shutdown();
        }
    }
}

pub(super) fn init() -> Result<DiagnosticsGuard, String> {
    let directory = choose_diagnostics_directory()?;
    let session_id = Uuid::new_v4();
    let service = DiagnosticsService::start(
        directory.clone(),
        session_id.to_string(),
        std::process::id(),
    )
    .map_err(|error| format!("start diagnostic service: {error}"))?;
    let sink = service.sink();
    let panic_writer = service.panic_writer();
    let active_path = service.active_path().to_path_buf();
    let service = Arc::new(Mutex::new(Some(service)));

    let filter = Targets::new()
        .with_default(LevelFilter::WARN)
        .with_target("clipline_app", LevelFilter::DEBUG)
        .with_target("clipline_capture", LevelFilter::DEBUG)
        .with_target("clipline_lol", LevelFilter::DEBUG)
        .with_target("clipline_storage", LevelFilter::DEBUG)
        .with_target("clipline_mp4", LevelFilter::DEBUG)
        .with_target("clipline_shell", LevelFilter::DEBUG);
    let layer = tracing_subscriber::fmt::layer()
        .json()
        .flatten_event(true)
        .with_target(true)
        .with_thread_ids(true)
        .with_thread_names(true)
        .with_current_span(true)
        .with_span_list(true)
        .with_span_events(FmtSpan::CLOSE)
        .with_writer(DiagnosticMakeWriter { sink });
    tracing_subscriber::registry()
        .with(filter)
        .with(layer)
        .try_init()
        .map_err(|error| format!("install diagnostic subscriber: {error}"))?;

    DIAGNOSTICS
        .set(DiagnosticsHandle {
            service: Arc::clone(&service),
            directory,
            active_path,
        })
        .map_err(|_| "diagnostics are already initialized".to_string())?;
    install_panic_hook(panic_writer);
    tracing::info!(
        event = "diagnostics_initialized",
        session_id = %session_id,
        generation_bytes = DIAGNOSTIC_GENERATION_BYTES,
        generations = DIAGNOSTIC_GENERATIONS,
        queue_lines = DIAGNOSTIC_QUEUE_LINES
    );
    Ok(DiagnosticsGuard { service })
}

pub(super) fn diagnostics_directory() -> Option<PathBuf> {
    DIAGNOSTICS.get().map(|handle| handle.directory.clone())
}

pub(super) fn diagnostic_log_path() -> Option<PathBuf> {
    DIAGNOSTICS.get().map(|handle| handle.active_path.clone())
}

pub(super) fn dropped_lines() -> usize {
    diagnostics_stats().map_or(0, |stats| stats.dropped_lines)
}

pub(super) fn write_errors() -> usize {
    diagnostics_stats().map_or(0, |stats| stats.write_errors)
}

fn diagnostics_stats() -> Option<clipline_shell::diagnostics::DiagnosticsStats> {
    let handle = DIAGNOSTICS.get()?;
    let service = handle.service.lock().ok()?;
    service.as_ref().map(DiagnosticsService::stats)
}

pub(super) fn snapshot_to(destination: &Path) -> Result<Vec<PathBuf>, String> {
    let handle = DIAGNOSTICS
        .get()
        .ok_or_else(|| "diagnostics are not initialized".to_string())?;
    let service = handle
        .service
        .lock()
        .map_err(|error| format!("lock diagnostic service: {error}"))?;
    service
        .as_ref()
        .ok_or_else(|| "diagnostics are already shut down".to_string())?
        .snapshot_to(destination)
        .map_err(|error| error.to_string())
}

pub(super) fn log_diagnostic(message: impl AsRef<str>) {
    tracing::debug!(
        event = "legacy_diagnostic",
        message = %single_line(message.as_ref())
    );
}

fn choose_diagnostics_directory() -> Result<PathBuf, String> {
    let preferred = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .map(|path| path.join("Clipline").join("logs"));
    if let Some(path) = preferred {
        if std::fs::create_dir_all(&path).is_ok() {
            return Ok(path);
        }
    }
    let fallback = std::env::temp_dir().join("Clipline").join("logs");
    std::fs::create_dir_all(&fallback)
        .map_err(|error| format!("create fallback diagnostic directory {fallback:?}: {error}"))?;
    Ok(fallback)
}

fn install_panic_hook(writer: PanicWriter) {
    PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            write_panic_record(&writer, info);
            previous(info);
        }));
    });
}

fn write_panic_record(writer: &PanicWriter, info: &std::panic::PanicHookInfo<'_>) {
    let payload = info
        .payload()
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
        .unwrap_or("<non-string panic payload>");
    let location = info
        .location()
        .map_or_else(|| "<unknown>".to_string(), ToString::to_string);
    let thread = std::thread::current();
    let thread_name = thread.name().unwrap_or("<unnamed>");
    let backtrace = Backtrace::force_capture().to_string();
    let _ = writer.write(PanicRecord {
        version: env!("CARGO_PKG_VERSION"),
        pid: std::process::id(),
        thread_name,
        location: &location,
        payload,
        backtrace: &backtrace,
    });
}

fn single_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}
