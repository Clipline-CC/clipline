//! Bounded, framework-neutral poster extraction and viewport ownership.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use image::{ImageFormat, ImageReader, Limits};
use thiserror::Error;

use crate::{
    ClipPathIdentity, DecodedImageWindow, DeterministicLru, GenerationError, PosterGeneration,
    WindowWorkToken, MAX_CATALOG_PAGE_ROWS, MAX_DECODED_PAGE_IMAGES, MAX_POSTER_RESULT_ENTRIES,
};

pub const POSTER_WIDTH: u32 = 480;
pub const POSTER_EXTRACTION_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_POSTER_ENCODED_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_POSTER_STDERR_BYTES: usize = 64 * 1024;
pub const MAX_CONCURRENT_POSTER_EXTRACTIONS: usize = 2;
pub const POSTER_NEGATIVE_RETRY: Duration = Duration::from_secs(30);
pub const MAX_POSTER_DIMENSION: u32 = 8_192;
pub const MAX_POSTER_DECODED_PIXELS: u64 = 1_048_576;
pub const MAX_POSTER_RGB_BYTES: usize = 3 * MAX_POSTER_DECODED_PIXELS as usize;
pub const MAX_POSTER_DECODER_ALLOC_BYTES: u64 = 16 * 1024 * 1024;

static NEXT_POSTER_TEMP_ID: AtomicU64 = AtomicU64::new(0);

pub type PosterExtractionResult = Result<PathBuf, String>;

type BoundedReaderHandle = std::thread::JoinHandle<io::Result<BoundedPipeOutput>>;

/// Process-independent extraction seam. Implementations return an owned,
/// file-backed poster path and must close every child and file handle first.
pub trait PosterExtractor: Send + Sync + 'static {
    fn extract(&self, canonical_clip: &Path, seek_seconds: f64) -> PosterExtractionResult;
}

pub trait FfmpegLocator: Send + Sync + 'static {
    fn locate(&self) -> Option<PathBuf>;
}

impl<F> FfmpegLocator for F
where
    F: Fn() -> Option<PathBuf> + Send + Sync + 'static,
{
    fn locate(&self) -> Option<PathBuf> {
        self()
    }
}

pub struct FfmpegPosterExtractor {
    locator: Arc<dyn FfmpegLocator>,
    successful_path: OnceLock<PathBuf>,
}

impl FfmpegPosterExtractor {
    #[must_use]
    pub fn new(locator: Arc<dyn FfmpegLocator>) -> Self {
        Self {
            locator,
            successful_path: OnceLock::new(),
        }
    }

    fn ffmpeg_path(&self) -> Option<PathBuf> {
        if let Some(path) = self.successful_path.get() {
            return Some(path.clone());
        }
        self.locator.locate()
    }
}

impl Default for FfmpegPosterExtractor {
    fn default() -> Self {
        Self::new(Arc::new(clipline_capture::ffmpeg::locate))
    }
}

impl PosterExtractor for FfmpegPosterExtractor {
    fn extract(&self, canonical_clip: &Path, seek_seconds: f64) -> PosterExtractionResult {
        let source = clipline_shell::open_regular_file_nofollow(canonical_clip)
            .map_err(|error| format!("open poster source: {error}"))?;
        let source_identity = clipline_shell::opened_file_identity(&source)
            .map_err(|error| format!("identify poster source: {error}"))?;
        if let Some(poster) = cached_poster_for_identity(canonical_clip, source_identity) {
            return Ok(poster);
        }
        let parent = canonical_clip
            .parent()
            .ok_or_else(|| "poster source has no parent".to_owned())?;
        let parent_authority = clipline_shell::DirectoryAuthority::open(parent)
            .map_err(|error| format!("open poster parent: {error}"))?;
        require_source_in_parent(canonical_clip, source_identity, &parent_authority)?;
        drop(source);
        let poster = poster_path(canonical_clip);
        let ffmpeg = self
            .ffmpeg_path()
            .ok_or_else(|| "ffmpeg is not available for poster extraction".to_owned())?;
        let published_identity = generate_poster(
            &ffmpeg,
            canonical_clip,
            &poster,
            seek_seconds,
            source_identity,
            &parent_authority,
        )?;
        let source_is_current = clipline_shell::open_regular_file_nofollow(canonical_clip)
            .and_then(|file| clipline_shell::opened_file_identity(&file))
            .is_ok_and(|current| current == source_identity);
        if !source_is_current {
            let _ = parent_authority.remove_file_if_identity(
                poster
                    .file_name()
                    .expect("poster has a generated file name"),
                published_identity,
            );
            return Err("poster source changed during extraction".to_owned());
        }
        let _ = self.successful_path.set(ffmpeg);
        Ok(poster)
    }
}

#[derive(Default)]
struct PermitState {
    active: usize,
    peak: usize,
}

struct PermitPool {
    limit: usize,
    state: Mutex<PermitState>,
    ready: Condvar,
}

fn process_poster_permits() -> Arc<PermitPool> {
    static PERMITS: OnceLock<Arc<PermitPool>> = OnceLock::new();
    Arc::clone(PERMITS.get_or_init(|| {
        Arc::new(PermitPool {
            limit: MAX_CONCURRENT_POSTER_EXTRACTIONS,
            state: Mutex::new(PermitState::default()),
            ready: Condvar::new(),
        })
    }))
}

impl PermitPool {
    fn acquire(self: &Arc<Self>) -> Result<ExtractionPermit, String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        while state.active >= self.limit {
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(|poison| poison.into_inner());
        }
        state.active += 1;
        state.peak = state.peak.max(state.active);
        drop(state);
        Ok(ExtractionPermit {
            pool: Arc::clone(self),
        })
    }

    fn peak(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .peak
    }
}

struct ExtractionPermit {
    pool: Arc<PermitPool>,
}

impl Drop for ExtractionPermit {
    fn drop(&mut self) {
        let mut state = self
            .pool
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.active = state.active.saturating_sub(1);
        self.pool.ready.notify_one();
    }
}

#[derive(Default)]
struct FlightState {
    result: Option<PosterExtractionResult>,
}

struct ExtractionFlight {
    state: Mutex<FlightState>,
    completed: Condvar,
}

impl ExtractionFlight {
    fn new() -> Self {
        Self {
            state: Mutex::new(FlightState::default()),
            completed: Condvar::new(),
        }
    }

    fn publish(&self, result: PosterExtractionResult) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.result = Some(result);
        self.completed.notify_all();
    }

    fn wait(&self) -> PosterExtractionResult {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        loop {
            if let Some(result) = state.result.clone() {
                return result;
            }
            state = self
                .completed
                .wait(state)
                .unwrap_or_else(|poison| poison.into_inner());
        }
    }
}

/// Canonical-path single-flight service with a process-wide two-worker bound.
///
/// The caller must provide a repository-contained regular file. The service
/// rejects a final symlink/reparse point before resolving a canonical key. The
/// FFmpeg subprocess must still reopen that path itself, so the service also
/// revalidates the selected file around extraction; this is the practical
/// ceiling for a path-based child process on Windows.
pub struct PosterService {
    extractor: Arc<dyn PosterExtractor>,
    permits: Arc<PermitPool>,
    flights: Mutex<HashMap<PathBuf, Arc<ExtractionFlight>>>,
}

impl PosterService {
    #[must_use]
    pub fn new(extractor: Arc<dyn PosterExtractor>) -> Self {
        Self {
            extractor,
            permits: process_poster_permits(),
            flights: Mutex::new(HashMap::new()),
        }
    }

    #[must_use]
    pub fn standard() -> Self {
        Self::new(Arc::new(FfmpegPosterExtractor::default()))
    }

    pub fn ensure_poster(&self, clip: &Path, seek_seconds: f64) -> PosterExtractionResult {
        let selected = clipline_shell::open_regular_file_nofollow(clip)
            .map_err(|error| format!("open poster clip without following links: {error}"))?;
        let selected_identity = clipline_shell::opened_file_identity(&selected)
            .map_err(|error| format!("identify poster clip: {error}"))?;
        let canonical_clip = clip
            .canonicalize()
            .map_err(|error| format!("canonicalize poster clip: {error}"))?;
        let canonical = clipline_shell::open_regular_file_nofollow(&canonical_clip)
            .map_err(|error| format!("open canonical poster clip: {error}"))?;
        if clipline_shell::opened_file_identity(&canonical)
            .map_err(|error| format!("identify canonical poster clip: {error}"))?
            != selected_identity
        {
            return Err("poster clip changed while it was selected".to_owned());
        }
        drop(canonical);
        drop(selected);
        if let Some(poster) = cached_poster_for_identity(&canonical_clip, selected_identity) {
            return Ok(poster);
        }
        let (flight, leader) = {
            let mut flights = self
                .flights
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if let Some(flight) = flights.get(&canonical_clip) {
                (Arc::clone(flight), false)
            } else {
                let flight = Arc::new(ExtractionFlight::new());
                flights.insert(canonical_clip.clone(), Arc::clone(&flight));
                (flight, true)
            }
        };
        if !leader {
            return flight.wait();
        }

        let result = match self.permits.acquire() {
            Ok(_permit) => std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.extractor.extract(&canonical_clip, seek_seconds)
            }))
            .unwrap_or_else(|_| Err("poster extraction worker panicked".to_owned())),
            Err(error) => Err(error),
        };
        let result = result.and_then(|poster| {
            if poster != poster_path(&canonical_clip) {
                return Err("poster extractor returned an unexpected path".to_owned());
            }
            let current_source = clipline_shell::open_regular_file_nofollow(&canonical_clip)
                .and_then(|file| clipline_shell::opened_file_identity(&file))
                .map_err(|error| format!("revalidate poster clip: {error}"))?;
            if current_source != selected_identity {
                return Err("poster clip changed during extraction".to_owned());
            }
            if cached_poster_for_identity(&canonical_clip, selected_identity).as_ref()
                != Some(&poster)
            {
                return Err("poster extractor returned an invalid cache artifact".to_owned());
            }
            Ok(poster)
        });
        flight.publish(result.clone());
        let mut flights = self
            .flights
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if flights
            .get(&canonical_clip)
            .is_some_and(|current| Arc::ptr_eq(current, &flight))
        {
            flights.remove(&canonical_clip);
        }
        result
    }

    #[must_use]
    pub fn peak_active_extractions(&self) -> usize {
        self.permits.peak()
    }
}

#[must_use]
pub fn poster_path(clip: &Path) -> PathBuf {
    clip.with_extension("poster.jpg")
}

#[must_use]
pub fn cached_poster(clip: &Path) -> Option<PathBuf> {
    let source = clipline_shell::open_regular_file_nofollow(clip).ok()?;
    let source_identity = clipline_shell::opened_file_identity(&source).ok()?;
    drop(source);
    cached_poster_for_identity(clip, source_identity)
}

fn cached_poster_for_identity(
    clip: &Path,
    expected_source_identity: clipline_shell::FileIdentity,
) -> Option<PathBuf> {
    let poster = poster_path(clip);
    poster_is_fresh(clip, expected_source_identity, &poster).then_some(poster)
}

/// Select the shared representative frame for both compatibility and native
/// galleries without allowing malformed sidecars to block a poster.
#[must_use]
pub fn poster_seek_seconds(clip: &Path) -> f64 {
    let Some(markers) = crate::read_marker_sidecar(clip)
        .ok()
        .flatten()
        .map(crate::ParsedMarkerSidecar::into_markers)
    else {
        return 1.0;
    };
    let duration_ok = markers.duration_s.is_finite() && markers.duration_s > 0.0;
    if let Some(first) = markers
        .markers
        .iter()
        .filter(|marker| clipline_events::is_review_event(&marker.event))
        .find(|marker| marker.event.involves_local_player)
        .or_else(|| {
            markers
                .markers
                .iter()
                .find(|marker| clipline_events::is_review_event(&marker.event))
        })
    {
        let time = first.t_s.max(0.0);
        return if duration_ok {
            time.min((markers.duration_s - 0.2).max(0.0))
        } else {
            time
        };
    }
    if duration_ok {
        (markers.duration_s * 0.15).min(5.0)
    } else {
        1.0
    }
}

fn poster_is_fresh(
    clip: &Path,
    expected_source_identity: clipline_shell::FileIdentity,
    poster: &Path,
) -> bool {
    let Ok(source) = clipline_shell::open_regular_file_nofollow(clip) else {
        return false;
    };
    let Ok(source_identity) = clipline_shell::opened_file_identity(&source) else {
        return false;
    };
    if source_identity != expected_source_identity {
        return false;
    }
    let Ok(clip_modified) = source.metadata().and_then(|metadata| metadata.modified()) else {
        return false;
    };
    let Ok(mut poster_file) = clipline_shell::open_regular_file_nofollow(poster) else {
        return false;
    };
    let Ok(poster_modified) = poster_file
        .metadata()
        .and_then(|metadata| metadata.modified())
    else {
        return false;
    };
    let Ok(output) = read_bounded(&mut poster_file, MAX_POSTER_ENCODED_BYTES) else {
        return false;
    };
    if output.overflowed || validate_poster_jpeg(&output.bytes).is_err() {
        return false;
    }
    let source_is_current = clipline_shell::open_regular_file_nofollow(clip)
        .and_then(|current| clipline_shell::opened_file_identity(&current))
        .is_ok_and(|current| current == expected_source_identity);
    source_is_current && poster_modified >= clip_modified
}

fn generate_poster(
    ffmpeg: &Path,
    clip: &Path,
    poster: &Path,
    seek_seconds: f64,
    expected_source_identity: clipline_shell::FileIdentity,
    parent_authority: &clipline_shell::DirectoryAuthority,
) -> Result<clipline_shell::FileIdentity, String> {
    let mut command = poster_command(ffmpeg, clip, seek_seconds);
    let output = run_poster_child(&mut command, POSTER_EXTRACTION_TIMEOUT)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr.bytes);
        return Err(format!("ffmpeg poster failed: {stderr}"));
    }
    require_source_in_parent(clip, expected_source_identity, parent_authority)?;
    publish_poster_output(poster, output.stdout, parent_authority)
}

fn require_source_in_parent(
    clip: &Path,
    expected_source_identity: clipline_shell::FileIdentity,
    parent_authority: &clipline_shell::DirectoryAuthority,
) -> Result<(), String> {
    if clip.parent() != Some(parent_authority.display_path()) {
        return Err("poster source is outside the selected parent authority".to_owned());
    }
    let name = clip
        .file_name()
        .ok_or_else(|| "poster source has no file name".to_owned())?;
    let current = parent_authority
        .regular_file_identity(name)
        .map_err(|error| format!("revalidate poster source in parent: {error}"))?;
    if current != Some(expected_source_identity) {
        return Err("poster source changed before publication".to_owned());
    }
    Ok(())
}

fn poster_command(ffmpeg: &Path, clip: &Path, seek_seconds: f64) -> Command {
    let mut command = Command::new(ffmpeg);
    clipline_capture::ffmpeg::suppress_console(&mut command);
    command
        .args([
            "-hide_banner",
            "-nostdin",
            "-ss",
            &format!("{:.3}", seek_seconds.max(0.0)),
            "-i",
        ])
        .arg(clip)
        .args([
            "-frames:v",
            "1",
            "-vf",
            "scale=480:-2:flags=bicubic",
            "-q:v",
            "4",
            "-c:v",
            "mjpeg",
            "-f",
            "image2pipe",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

enum PosterChildWait {
    Exited(ExitStatus),
    TimedOut,
}

fn kill_and_reap_poster_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

struct BoundedPipeOutput {
    bytes: Vec<u8>,
    overflowed: bool,
}

struct PosterChildOutput {
    status: ExitStatus,
    stdout: BoundedPipeOutput,
    stderr: BoundedPipeOutput,
}

fn wait_for_poster_child(child: &mut Child, timeout: Duration) -> io::Result<PosterChildWait> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(PosterChildWait::Exited(status)),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                kill_and_reap_poster_child(child);
                return Ok(PosterChildWait::TimedOut);
            }
            Err(error) => {
                kill_and_reap_poster_child(child);
                return Err(error);
            }
        }
    }
}

fn run_poster_child(command: &mut Command, timeout: Duration) -> Result<PosterChildOutput, String> {
    run_poster_child_with_limits(
        command,
        timeout,
        MAX_POSTER_ENCODED_BYTES,
        MAX_POSTER_STDERR_BYTES,
    )
}

fn run_poster_child_with_limits(
    command: &mut Command,
    timeout: Duration,
    max_stdout_bytes: usize,
    max_stderr_bytes: usize,
) -> Result<PosterChildOutput, String> {
    run_poster_child_with_reader_spawner(
        command,
        timeout,
        max_stdout_bytes,
        max_stderr_bytes,
        spawn_bounded_reader,
    )
}

fn run_poster_child_with_reader_spawner(
    command: &mut Command,
    timeout: Duration,
    max_stdout_bytes: usize,
    max_stderr_bytes: usize,
    mut spawn_reader: impl FnMut(&str, Box<dyn Read + Send>, usize) -> io::Result<BoundedReaderHandle>,
) -> Result<PosterChildOutput, String> {
    let mut child = command
        .spawn()
        .map_err(|error| format!("spawn ffmpeg poster: {error}"))?;
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        kill_and_reap_poster_child(&mut child);
        return Err("spawn ffmpeg poster: stdout or stderr pipe unavailable".to_owned());
    };
    let stdout_reader =
        match spawn_reader("clipline-poster-stdout", Box::new(stdout), max_stdout_bytes) {
            Ok(reader) => reader,
            Err(error) => {
                kill_and_reap_poster_child(&mut child);
                return Err(format!("spawn ffmpeg poster stdout reader: {error}"));
            }
        };
    let stderr_reader =
        match spawn_reader("clipline-poster-stderr", Box::new(stderr), max_stderr_bytes) {
            Ok(reader) => reader,
            Err(error) => {
                kill_and_reap_poster_child(&mut child);
                let _ = stdout_reader.join();
                return Err(format!("spawn ffmpeg poster stderr reader: {error}"));
            }
        };
    let wait = wait_for_poster_child(&mut child, timeout);
    // Join both readers before propagating either result. Both pipes are
    // owned solely by the reaped child, so EOF is guaranteed after `wait`.
    let stdout = join_bounded_reader(stdout_reader, "stdout");
    let stderr = join_bounded_reader(stderr_reader, "stderr");
    let stdout = stdout?;
    let stderr = stderr?;
    match wait.map_err(|error| format!("wait for ffmpeg poster: {error}"))? {
        PosterChildWait::Exited(status) => Ok(PosterChildOutput {
            status,
            stdout,
            stderr,
        }),
        PosterChildWait::TimedOut => Err(format!(
            "ffmpeg poster timed out after {} seconds",
            timeout.as_secs()
        )),
    }
}

fn spawn_bounded_reader(
    name: &str,
    reader: Box<dyn Read + Send>,
    maximum: usize,
) -> io::Result<BoundedReaderHandle> {
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || read_bounded(reader, maximum))
}

fn join_bounded_reader(
    reader: std::thread::JoinHandle<io::Result<BoundedPipeOutput>>,
    stream: &str,
) -> Result<BoundedPipeOutput, String> {
    reader
        .join()
        .map_err(|_| format!("ffmpeg poster {stream} reader panicked"))?
        .map_err(|error| format!("read ffmpeg poster {stream}: {error}"))
}

fn read_bounded(mut reader: impl Read, maximum: usize) -> io::Result<BoundedPipeOutput> {
    let mut bytes = Vec::new();
    let mut overflowed = false;
    let mut chunk = [0_u8; 8 * 1024];
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            return Ok(BoundedPipeOutput { bytes, overflowed });
        }
        let keep = read.min(maximum.saturating_sub(bytes.len()));
        bytes.extend_from_slice(&chunk[..keep]);
        overflowed |= keep < read;
    }
}

fn publish_poster_output(
    poster: &Path,
    output: BoundedPipeOutput,
    parent_authority: &clipline_shell::DirectoryAuthority,
) -> Result<clipline_shell::FileIdentity, String> {
    if output.overflowed {
        return Err(format!(
            "ffmpeg poster exceeded the {MAX_POSTER_ENCODED_BYTES}-byte output limit"
        ));
    }
    validate_poster_jpeg(&output.bytes)?;
    let mut temporary = PosterTemp::reserve(poster, parent_authority)?;
    temporary.write_all(&output.bytes)?;
    temporary.publish(poster)
}

fn validate_poster_jpeg(bytes: &[u8]) -> Result<(), String> {
    if !bytes.starts_with(&[0xff, 0xd8]) || !bytes.ends_with(&[0xff, 0xd9]) {
        return Err("ffmpeg poster did not produce a complete JPEG envelope".to_owned());
    }
    let reader = ImageReader::with_format(Cursor::new(bytes), ImageFormat::Jpeg);
    let (width, height) = reader
        .into_dimensions()
        .map_err(|_| "ffmpeg poster produced corrupt JPEG data".to_owned())?;
    if width == 0 || height == 0 || width > MAX_POSTER_DIMENSION || height > MAX_POSTER_DIMENSION {
        return Err("ffmpeg poster dimensions exceed their bound".to_owned());
    }
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| "ffmpeg poster pixel count overflowed".to_owned())?;
    if pixels > MAX_POSTER_DECODED_PIXELS {
        return Err("ffmpeg poster pixel count exceeds its bound".to_owned());
    }
    let expected_rgb = pixels
        .checked_mul(3)
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or_else(|| "ffmpeg poster RGB byte count overflowed".to_owned())?;
    if expected_rgb > MAX_POSTER_RGB_BYTES {
        return Err("ffmpeg poster RGB byte count exceeds its bound".to_owned());
    }

    let mut reader = ImageReader::with_format(Cursor::new(bytes), ImageFormat::Jpeg);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_POSTER_DIMENSION);
    limits.max_image_height = Some(MAX_POSTER_DIMENSION);
    limits.max_alloc = Some(MAX_POSTER_DECODER_ALLOC_BYTES);
    reader.limits(limits);
    let rgb = reader
        .decode()
        .map_err(|_| "ffmpeg poster produced corrupt JPEG data".to_owned())?
        .into_rgb8();
    if rgb.width() != width || rgb.height() != height || rgb.as_raw().len() != expected_rgb {
        return Err("ffmpeg poster decoded shape changed".to_owned());
    }
    Ok(())
}

struct PosterTemp<'authority> {
    authority: &'authority clipline_shell::DirectoryAuthority,
    name: OsString,
    file: Option<File>,
    identity: clipline_shell::FileIdentity,
    expected_target_identity: Option<clipline_shell::FileIdentity>,
    armed: bool,
}

impl<'authority> PosterTemp<'authority> {
    fn reserve(
        poster: &Path,
        authority: &'authority clipline_shell::DirectoryAuthority,
    ) -> Result<Self, String> {
        if poster.parent() != Some(authority.display_path()) {
            return Err("poster path is outside the selected parent authority".to_owned());
        }
        let file_name = poster
            .file_name()
            .ok_or_else(|| "poster path has no file name".to_owned())?;
        let expected_target_identity = authority
            .regular_file_identity(file_name)
            .map_err(|error| format!("inspect stale poster: {error}"))?;
        for _ in 0..64 {
            let id = NEXT_POSTER_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let mut name = file_name.to_os_string();
            name.push(format!(".tmp.{}.{id}", std::process::id()));
            match authority.create_new_regular_file(&name) {
                Ok(file) => {
                    let (file, identity) = identify_reserved_temp(file, authority, &name)?;
                    return Ok(Self {
                        authority,
                        name,
                        file: Some(file),
                        identity,
                        expected_target_identity,
                        armed: true,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(format!("reserve poster temp: {error}")),
            }
        }
        Err("reserve poster temp: unique-name attempts exhausted".to_owned())
    }

    fn write_all(&mut self, jpeg: &[u8]) -> Result<(), String> {
        self.file
            .as_mut()
            .ok_or_else(|| "poster temp is already closed".to_owned())?
            .write_all(jpeg)
            .and_then(|()| self.file.as_ref().expect("poster temp open").sync_all())
            .map_err(|error| format!("write poster temp: {error}"))
    }

    fn publish(mut self, poster: &Path) -> Result<clipline_shell::FileIdentity, String> {
        if poster.parent() != Some(self.authority.display_path()) {
            return Err("poster publish path is outside the selected parent authority".to_owned());
        }
        let poster_name = poster
            .file_name()
            .ok_or_else(|| "poster publish path has no file name".to_owned())?;
        drop(self.file.take());
        match self.expected_target_identity {
            Some(target_identity) => {
                self.authority
                    .replace_file_if_identities(
                        &self.name,
                        self.identity,
                        poster_name,
                        target_identity,
                    )
                    .map_err(|error| format!("finalize poster: {error}"))?;
            }
            None => {
                self.authority
                    .rename_file_noreplace_if_identity(&self.name, poster_name, self.identity)
                    .map_err(|error| format!("finalize poster: {error}"))?;
            }
        }
        self.armed = false;
        Ok(self.identity)
    }
}

fn identify_reserved_temp(
    file: File,
    authority: &clipline_shell::DirectoryAuthority,
    name: &std::ffi::OsStr,
) -> Result<(File, clipline_shell::FileIdentity), String> {
    identify_reserved_temp_with(file, authority, name, clipline_shell::opened_file_identity)
}

fn identify_reserved_temp_with(
    file: File,
    authority: &clipline_shell::DirectoryAuthority,
    name: &std::ffi::OsStr,
    identify: impl FnOnce(&File) -> io::Result<clipline_shell::FileIdentity>,
) -> Result<(File, clipline_shell::FileIdentity), String> {
    match identify(&file) {
        Ok(identity) => Ok((file, identity)),
        Err(error) => {
            // Keep cleanup identity-fenced even on this early return. A retry
            // while the exclusively-created handle is still open fixes a
            // transient identity-query failure without risking deletion of a
            // replacement at the same path.
            let cleanup_identity = clipline_shell::opened_file_identity(&file).ok();
            drop(file);
            if let Some(identity) = cleanup_identity {
                let _ = authority.remove_file_if_identity(name, identity);
            }
            Err(format!("identify poster temp: {error}"))
        }
    }
}

impl Drop for PosterTemp<'_> {
    fn drop(&mut self) {
        drop(self.file.take());
        if self.armed {
            let _ = self
                .authority
                .remove_file_if_identity(&self.name, self.identity);
        }
    }
}

#[cfg(test)]
impl PosterTemp<'_> {
    fn path(&self) -> PathBuf {
        self.authority.display_path().join(&self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct PosterWorkToken {
    pub window: WindowWorkToken,
    pub poster: PosterGeneration,
    pub path: ClipPathIdentity,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PosterPageItem {
    pub identity: ClipPathIdentity,
    pub native_path: PathBuf,
    pub source_identity: Option<clipline_shell::FileIdentity>,
    pub seek_seconds: f64,
}

impl PosterPageItem {
    pub fn new(native_path: PathBuf, seek_seconds: f64) -> Result<Self, PosterControllerError> {
        Self::new_with_file_identity(native_path, None, seek_seconds)
    }

    pub fn new_with_file_identity(
        native_path: PathBuf,
        source_identity: Option<clipline_shell::FileIdentity>,
        seek_seconds: f64,
    ) -> Result<Self, PosterControllerError> {
        let identity =
            ClipPathIdentity::from_path(&native_path).ok_or(PosterControllerError::InvalidPath)?;
        if !seek_seconds.is_finite() || seek_seconds < 0.0 {
            return Err(PosterControllerError::InvalidSeek);
        }
        Ok(Self {
            identity,
            native_path,
            source_identity,
            seek_seconds,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PosterWorkKind {
    Extract,
    Decode { encoded_path: PathBuf },
}

#[derive(Debug, Clone, PartialEq)]
pub struct PosterWorkRequest {
    pub token: PosterWorkToken,
    pub item: PosterPageItem,
    pub kind: PosterWorkKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PosterCompletion {
    Ready(PathBuf),
    Missing,
    Failed(PosterFailureKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PosterFailureKind {
    Unavailable,
    Corrupt,
    TimedOut,
    Io,
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum PosterControllerError {
    #[error("poster page exceeds its row limit")]
    PageTooLarge,
    #[error("poster page contains a duplicate path")]
    DuplicatePath,
    #[error("poster path is invalid")]
    InvalidPath,
    #[error("poster seek is invalid")]
    InvalidSeek,
    #[error(transparent)]
    Generation(#[from] GenerationError),
}

#[derive(Debug)]
pub struct PosterControllerUpdate<H> {
    pub queued: Vec<PosterWorkRequest>,
    pub canceled: Vec<PosterWorkRequest>,
    pub released: Vec<H>,
}

impl<H> Default for PosterControllerUpdate<H> {
    fn default() -> Self {
        Self {
            queued: Vec::new(),
            canceled: Vec::new(),
            released: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PosterCacheEntry {
    Ready {
        path: PathBuf,
        source_identity: Option<clipline_shell::FileIdentity>,
    },
    Negative {
        failure: PosterNegativeResult,
        retry_at: Instant,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PosterNegativeResult {
    Missing,
    Failed(PosterFailureKind),
}

#[derive(Debug)]
enum PosterSlot<H> {
    Pending(PosterWorkRequest),
    Ready(H),
}

/// UI-thread controller for the bounded decoded-image ownership window.
pub struct PosterController<H> {
    window: Option<WindowWorkToken>,
    poster_generation: PosterGeneration,
    page: Vec<PosterPageItem>,
    image_window: DecodedImageWindow,
    cache: DeterministicLru<ClipPathIdentity, PosterCacheEntry>,
    slots: BTreeMap<ClipPathIdentity, PosterSlot<H>>,
    issued: Vec<PosterWorkRequest>,
}

impl<H> PosterController<H> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            window: None,
            poster_generation: PosterGeneration::INITIAL,
            page: Vec::new(),
            image_window: DecodedImageWindow::default(),
            cache: DeterministicLru::new(MAX_POSTER_RESULT_ENTRIES),
            slots: BTreeMap::new(),
            issued: Vec::with_capacity(MAX_DECODED_PAGE_IMAGES),
        }
    }

    pub fn replace_page(
        &mut self,
        window: WindowWorkToken,
        page: Vec<PosterPageItem>,
    ) -> Result<PosterControllerUpdate<H>, PosterControllerError> {
        if page.len() > MAX_CATALOG_PAGE_ROWS {
            return Err(PosterControllerError::PageTooLarge);
        }
        let mut identities: Vec<_> = page.iter().map(|item| item.identity.clone()).collect();
        identities.sort();
        if identities.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(PosterControllerError::DuplicatePath);
        }
        if page.iter().any(|item| {
            ClipPathIdentity::from_path(&item.native_path).as_ref() != Some(&item.identity)
                || !item.seek_seconds.is_finite()
                || item.seek_seconds < 0.0
        }) {
            return Err(PosterControllerError::InvalidPath);
        }
        let update = self.clear_slots();
        self.window = Some(window);
        self.page = page;
        self.image_window = DecodedImageWindow::default();
        Ok(update)
    }

    pub fn set_viewport(
        &mut self,
        visible_start: usize,
        visible_count: usize,
        overscan: usize,
    ) -> Result<PosterControllerUpdate<H>, PosterControllerError> {
        self.set_viewport_at(visible_start, visible_count, overscan, Instant::now())
    }

    pub fn set_viewport_at(
        &mut self,
        visible_start: usize,
        visible_count: usize,
        overscan: usize,
        now: Instant,
    ) -> Result<PosterControllerUpdate<H>, PosterControllerError> {
        let image_window =
            DecodedImageWindow::around(self.page.len(), visible_start, visible_count, overscan);
        let next_items = &self.page[image_window.start()..image_window.end()];
        let next_identities: Vec<_> = next_items
            .iter()
            .map(|item| item.identity.clone())
            .collect();
        self.preflight_viewport_generations(image_window, now, &next_identities)?;
        let mut update = PosterControllerUpdate::default();
        let old_slots = std::mem::take(&mut self.slots);
        for (identity, slot) in old_slots {
            match slot {
                PosterSlot::Pending(request) => update.canceled.push(request),
                PosterSlot::Ready(handle) if next_identities.contains(&identity) => {
                    self.slots.insert(identity, PosterSlot::Ready(handle));
                }
                PosterSlot::Ready(handle) => update.released.push(handle),
            }
        }
        self.image_window = image_window;
        self.reconcile(now, &mut update)?;
        Ok(update)
    }

    #[must_use]
    pub fn accept_extracted(
        &mut self,
        request: &PosterWorkRequest,
        completion: PosterCompletion,
    ) -> PosterControllerUpdate<H> {
        self.accept_extracted_at(request, completion, Instant::now())
            .unwrap_or_default()
    }

    pub fn accept_extracted_at(
        &mut self,
        request: &PosterWorkRequest,
        completion: PosterCompletion,
        now: Instant,
    ) -> Result<PosterControllerUpdate<H>, PosterControllerError> {
        let mut update = PosterControllerUpdate::default();
        if !matches!(request.kind, PosterWorkKind::Extract) || !self.finish_issued(request) {
            return Ok(update);
        }
        let current = self.request_is_current(request);
        if current {
            self.slots.remove(&request.item.identity);
        } else {
            self.reconcile(now, &mut update)?;
            return Ok(update);
        }
        let cache = match completion {
            PosterCompletion::Ready(path) if path == poster_path(&request.item.native_path) => {
                PosterCacheEntry::Ready {
                    path,
                    source_identity: request.item.source_identity,
                }
            }
            PosterCompletion::Ready(_) => PosterCacheEntry::Negative {
                failure: PosterNegativeResult::Failed(PosterFailureKind::Corrupt),
                retry_at: retry_at(now),
            },
            PosterCompletion::Missing => PosterCacheEntry::Negative {
                failure: PosterNegativeResult::Missing,
                retry_at: retry_at(now),
            },
            PosterCompletion::Failed(failure) => PosterCacheEntry::Negative {
                failure: PosterNegativeResult::Failed(failure),
                retry_at: retry_at(now),
            },
        };
        self.cache.insert(request.item.identity.clone(), cache);
        self.reconcile(now, &mut update)?;
        Ok(update)
    }

    #[must_use]
    pub fn accept_decoded(
        &mut self,
        request: &PosterWorkRequest,
        handle: H,
    ) -> PosterControllerUpdate<H> {
        let mut update = PosterControllerUpdate::default();
        let current = matches!(request.kind, PosterWorkKind::Decode { .. })
            && self.issued_contains(request)
            && self.request_is_current(request);
        if !self.finish_issued(request) || !current {
            update.released.push(handle);
            let _ = self.reconcile(Instant::now(), &mut update);
            return update;
        }
        self.slots
            .insert(request.item.identity.clone(), PosterSlot::Ready(handle));
        update
    }

    pub fn accept_decoded_with(
        &mut self,
        request: &PosterWorkRequest,
        make_handle: impl FnOnce() -> H,
    ) -> Result<PosterControllerUpdate<H>, PosterControllerError> {
        let mut update = PosterControllerUpdate::default();
        let current = matches!(request.kind, PosterWorkKind::Decode { .. })
            && self.issued_contains(request)
            && self.request_is_current(request);
        if !self.finish_issued(request) || !current {
            self.reconcile(Instant::now(), &mut update)?;
            return Ok(update);
        }
        self.slots.insert(
            request.item.identity.clone(),
            PosterSlot::Ready(make_handle()),
        );
        Ok(update)
    }

    pub fn accept_decode_failed(
        &mut self,
        request: &PosterWorkRequest,
        failure: PosterFailureKind,
    ) -> Result<PosterControllerUpdate<H>, PosterControllerError> {
        let mut update = PosterControllerUpdate::default();
        let current = self.request_is_current(request);
        if !self.finish_issued(request) {
            return Ok(update);
        }
        if current {
            self.slots.remove(&request.item.identity);
            self.cache.insert(
                request.item.identity.clone(),
                PosterCacheEntry::Negative {
                    failure: PosterNegativeResult::Failed(failure),
                    retry_at: retry_at(Instant::now()),
                },
            );
        }
        self.reconcile(Instant::now(), &mut update)?;
        Ok(update)
    }

    pub fn acknowledge_canceled(
        &mut self,
        request: &PosterWorkRequest,
    ) -> Result<PosterControllerUpdate<H>, PosterControllerError> {
        let mut update = PosterControllerUpdate::default();
        if self.request_is_current(request) {
            return Ok(update);
        }
        self.finish_issued(request);
        self.reconcile(Instant::now(), &mut update)?;
        Ok(update)
    }

    pub fn invalidate_path(
        &mut self,
        identity: &ClipPathIdentity,
    ) -> Result<PosterControllerUpdate<H>, PosterControllerError> {
        let mut update = PosterControllerUpdate::default();
        if let Some(slot) = self.slots.remove(identity) {
            match slot {
                PosterSlot::Pending(request) => update.canceled.push(request),
                PosterSlot::Ready(handle) => update.released.push(handle),
            }
        }
        self.page.retain(|item| &item.identity != identity);
        self.cache.remove(identity);
        self.image_window = DecodedImageWindow::around(
            self.page.len(),
            self.image_window.start(),
            self.image_window.len(),
            0,
        );
        self.reconcile(Instant::now(), &mut update)?;
        Ok(update)
    }

    pub fn hide(&mut self) -> Result<PosterControllerUpdate<H>, PosterControllerError> {
        let update = self.clear_slots();
        self.window = None;
        self.image_window = DecodedImageWindow::default();
        Ok(update)
    }

    pub fn detach_window(&mut self) -> Result<PosterControllerUpdate<H>, PosterControllerError> {
        let update = self.hide()?;
        self.page.clear();
        Ok(update)
    }

    pub fn detach_window_if_matches(
        &mut self,
        window: WindowWorkToken,
    ) -> Result<PosterControllerUpdate<H>, PosterControllerError> {
        if self.window != Some(window) {
            return Ok(PosterControllerUpdate::default());
        }
        self.detach_window()
    }

    fn request_is_current(&self, request: &PosterWorkRequest) -> bool {
        self.window == Some(request.token.window)
            && request.token.path == request.item.identity
            && self.slots.get(&request.item.identity).is_some_and(
                |slot| matches!(slot, PosterSlot::Pending(current) if current == request),
            )
    }

    fn issued_contains(&self, request: &PosterWorkRequest) -> bool {
        self.issued.iter().any(|current| current == request)
    }

    fn finish_issued(&mut self, request: &PosterWorkRequest) -> bool {
        let Some(position) = self.issued.iter().position(|current| current == request) else {
            return false;
        };
        self.issued.remove(position);
        true
    }

    fn reconcile(
        &mut self,
        now: Instant,
        update: &mut PosterControllerUpdate<H>,
    ) -> Result<(), PosterControllerError> {
        let Some(window) = self.window else {
            return Ok(());
        };
        let items = self.page[self.image_window.start()..self.image_window.end()].to_vec();
        let mut generation = self.poster_generation;
        let mut planned = Vec::new();
        for item in items {
            if self.ownership_count().saturating_add(planned.len()) >= MAX_DECODED_PAGE_IMAGES {
                break;
            }
            if self.slots.contains_key(&item.identity)
                || self
                    .issued
                    .iter()
                    .any(|request| request.item.identity == item.identity)
            {
                continue;
            }
            let cached = self.cache.peek(&item.identity).cloned();
            let (kind, cache_action) = match cached {
                Some(PosterCacheEntry::Ready {
                    path: encoded_path,
                    source_identity,
                }) if source_identity == item.source_identity => {
                    (PosterWorkKind::Decode { encoded_path }, CacheAction::Touch)
                }
                Some(PosterCacheEntry::Ready { .. }) => {
                    (PosterWorkKind::Extract, CacheAction::Remove)
                }
                Some(PosterCacheEntry::Negative { retry_at, .. }) if retry_at > now => continue,
                Some(PosterCacheEntry::Negative { .. }) => {
                    (PosterWorkKind::Extract, CacheAction::Remove)
                }
                None => (PosterWorkKind::Extract, CacheAction::None),
            };
            let poster = generation.checked_next()?;
            generation = poster;
            planned.push((item, kind, cache_action, poster));
        }
        self.poster_generation = generation;
        for (item, kind, cache_action, poster) in planned {
            match cache_action {
                CacheAction::Touch => {
                    let _ = self.cache.get(&item.identity);
                }
                CacheAction::Remove => {
                    self.cache.remove(&item.identity);
                }
                CacheAction::None => {}
            }
            let request = PosterWorkRequest {
                token: PosterWorkToken {
                    window,
                    poster,
                    path: item.identity.clone(),
                },
                item,
                kind,
            };
            self.issued.push(request.clone());
            self.slots.insert(
                request.item.identity.clone(),
                PosterSlot::Pending(request.clone()),
            );
            update.queued.push(request);
        }
        debug_assert!(self.ownership_count() <= MAX_DECODED_PAGE_IMAGES);
        Ok(())
    }

    fn preflight_viewport_generations(
        &self,
        image_window: DecodedImageWindow,
        now: Instant,
        next_identities: &[ClipPathIdentity],
    ) -> Result<(), PosterControllerError> {
        let retained = self
            .slots
            .iter()
            .filter(|(identity, slot)| {
                next_identities.contains(identity) && matches!(slot, PosterSlot::Ready(_))
            })
            .count();
        let capacity =
            MAX_DECODED_PAGE_IMAGES.saturating_sub(self.issued.len().saturating_add(retained));
        let mut needed = 0_usize;
        for item in &self.page[image_window.start()..image_window.end()] {
            if needed >= capacity
                || self
                    .slots
                    .get(&item.identity)
                    .is_some_and(|slot| matches!(slot, PosterSlot::Ready(_)))
                || self
                    .issued
                    .iter()
                    .any(|request| request.item.identity == item.identity)
            {
                continue;
            }
            if self.cache.peek(&item.identity).is_some_and(
                |entry| matches!(entry, PosterCacheEntry::Negative { retry_at, .. } if *retry_at > now),
            ) {
                continue;
            }
            needed += 1;
        }
        let mut generation = self.poster_generation;
        for _ in 0..needed {
            generation = generation.checked_next()?;
        }
        Ok(())
    }

    fn clear_slots(&mut self) -> PosterControllerUpdate<H> {
        let mut update = PosterControllerUpdate::default();
        for (_, slot) in std::mem::take(&mut self.slots) {
            match slot {
                PosterSlot::Pending(request) => update.canceled.push(request),
                PosterSlot::Ready(handle) => update.released.push(handle),
            }
        }
        update
    }

    #[must_use]
    pub fn page_len(&self) -> usize {
        self.page.len()
    }

    #[must_use]
    pub fn cache_len(&self) -> usize {
        self.cache.len()
    }

    #[must_use]
    pub fn cache_contains(&self, identity: &ClipPathIdentity) -> bool {
        self.cache.contains_key(identity)
    }

    #[must_use]
    pub fn retained_image_count(&self) -> usize {
        self.slots
            .values()
            .filter(|slot| matches!(slot, PosterSlot::Ready(_)))
            .count()
    }

    /// Borrow the decoded image retained for one exact clip identity.
    ///
    /// Retained images only exist inside the bounded decoded-image window, so
    /// callers cannot use this accessor to expand ownership beyond
    /// [`MAX_DECODED_PAGE_IMAGES`].
    #[must_use]
    pub fn retained_image(&self, identity: &ClipPathIdentity) -> Option<&H> {
        match self.slots.get(identity) {
            Some(PosterSlot::Ready(handle)) => Some(handle),
            Some(PosterSlot::Pending(_)) | None => None,
        }
    }

    #[must_use]
    pub fn queued_work_count(&self) -> usize {
        self.slots
            .values()
            .filter(|slot| matches!(slot, PosterSlot::Pending(_)))
            .count()
    }

    #[must_use]
    pub fn ownership_count(&self) -> usize {
        self.issued
            .len()
            .saturating_add(self.retained_image_count())
    }

    #[must_use]
    pub fn accepts_request(&self, request: &PosterWorkRequest) -> bool {
        self.issued_contains(request) && self.request_is_current(request)
    }
}

#[derive(Debug, Clone, Copy)]
enum CacheAction {
    None,
    Touch,
    Remove,
}

fn retry_at(now: Instant) -> Instant {
    now.checked_add(POSTER_NEGATIVE_RETRY).unwrap_or(now)
}

impl<H> Default for PosterController<H> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_jpeg() -> Vec<u8> {
        let image = image::RgbImage::from_raw(2, 2, vec![0x55; 12]).unwrap();
        let mut encoded = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut encoded, image::ImageFormat::Jpeg)
            .unwrap();
        encoded.into_inner()
    }

    fn test_window() -> WindowWorkToken {
        WindowWorkToken {
            attachment: crate::WindowAttachmentGeneration::new(1),
            foreground: crate::ForegroundGeneration::new(1),
            request: crate::RequestGeneration::new(1),
        }
    }

    fn test_item(index: usize) -> PosterPageItem {
        PosterPageItem::new(PathBuf::from(format!(r"C:\clips\clip-{index}.mp4")), 0.0).unwrap()
    }

    #[test]
    fn ffmpeg_locator_retries_misses_and_only_a_success_is_cached() {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let locator_attempts = Arc::clone(&attempts);
        let extractor = FfmpegPosterExtractor::new(Arc::new(move || {
            let attempt = locator_attempts.fetch_add(1, Ordering::SeqCst);
            (attempt > 0).then(|| PathBuf::from("ffmpeg-success"))
        }));

        assert_eq!(extractor.ffmpeg_path(), None);
        assert_eq!(
            extractor.ffmpeg_path(),
            Some(PathBuf::from("ffmpeg-success"))
        );
        assert!(extractor.successful_path.get().is_none());
        extractor
            .successful_path
            .set(PathBuf::from("ffmpeg-success"))
            .unwrap();
        assert_eq!(
            extractor.ffmpeg_path(),
            Some(PathBuf::from("ffmpeg-success"))
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn generation_exhaustion_does_not_release_the_current_viewport_handle() {
        let mut controller = PosterController::<u64>::new();
        controller
            .replace_page(test_window(), vec![test_item(0), test_item(1)])
            .unwrap();
        let extract = controller.set_viewport(0, 1, 0).unwrap().queued.remove(0);
        let decode = controller.accept_extracted(
            &extract,
            PosterCompletion::Ready(poster_path(&extract.item.native_path)),
        );
        let decode = decode.queued[0].clone();
        let _ = controller.accept_decoded(&decode, 7);
        controller.poster_generation = PosterGeneration::new(u64::MAX);

        assert!(matches!(
            controller.set_viewport(1, 1, 0),
            Err(PosterControllerError::Generation(_))
        ));
        assert_eq!(controller.retained_image_count(), 1);
        assert_eq!(controller.ownership_count(), 1);
        assert_eq!(controller.image_window.start(), 0);
        assert_eq!(controller.image_window.end(), 1);
    }

    #[test]
    fn poster_command_streams_one_bounded_mjpeg_to_stdout() {
        let clip = Path::new(r"C:\clips\clip.mp4");
        let command = poster_command(Path::new(r"C:\tools\ffmpeg.exe"), clip, -2.5);
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(args
            .windows(2)
            .any(|pair| pair[0] == "-ss" && pair[1] == "0.000"));
        assert!(args
            .windows(2)
            .any(|pair| pair[0] == "-vf" && pair[1] == "scale=480:-2:flags=bicubic"));
        assert!(args
            .windows(2)
            .any(|pair| pair[0] == "-c:v" && pair[1] == "mjpeg"));
        assert_eq!(args.last().map(String::as_str), Some("pipe:1"));
        assert!(!args.iter().any(|arg| arg.ends_with(".poster.jpg")));
    }

    #[test]
    fn bounded_reader_retains_a_prefix_and_drains_to_eof() {
        let output = read_bounded(io::Cursor::new(b"abcdefgh"), 3).unwrap();
        assert_eq!(output.bytes, b"abc");
        assert!(output.overflowed);
    }

    #[test]
    fn a_source_replaced_after_selection_cannot_reuse_the_old_cache() {
        let directory = test_directory("cache-source-fence");
        let clip = directory.join("clip.mp4");
        let replacement = directory.join("replacement.mp4");
        let poster = poster_path(&clip);
        std::fs::write(&clip, b"selected source").unwrap();
        std::fs::write(&replacement, b"pre-existing replacement source").unwrap();
        std::fs::write(&poster, test_jpeg()).unwrap();
        let selected = clipline_shell::open_regular_file_nofollow(&clip).unwrap();
        let selected_identity = clipline_shell::opened_file_identity(&selected).unwrap();
        drop(selected);

        std::fs::remove_file(&clip).unwrap();
        std::fs::rename(&replacement, &clip).unwrap();

        assert_eq!(cached_poster_for_identity(&clip, selected_identity), None);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn corrupt_jpeg_envelope_never_becomes_a_fresh_durable_cache_hit() {
        let directory = test_directory("corrupt-cache-hit");
        let clip = directory.join("clip.mp4");
        let poster = poster_path(&clip);
        std::fs::write(&clip, b"clip").unwrap();
        std::fs::write(&poster, b"\xff\xd8corrupt entropy\xff\xd9").unwrap();

        assert_eq!(cached_poster(&clip), None);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_parent_swapped_before_authority_selection_cannot_touch_foreign_posters() {
        let container = test_directory("source-parent-binding");
        let selected_parent = container.join("selected");
        let moved_parent = container.join("moved");
        std::fs::create_dir(&selected_parent).unwrap();
        let clip = selected_parent.join("clip.mp4");
        std::fs::write(&clip, b"selected source").unwrap();
        let selected = clipline_shell::open_regular_file_nofollow(&clip).unwrap();
        let selected_identity = clipline_shell::opened_file_identity(&selected).unwrap();
        drop(selected);

        std::fs::rename(&selected_parent, &moved_parent).unwrap();
        std::fs::create_dir(&selected_parent).unwrap();
        std::fs::write(&clip, b"foreign source").unwrap();
        let foreign_poster = poster_path(&clip);
        std::fs::write(&foreign_poster, b"foreign poster").unwrap();
        let authority = clipline_shell::DirectoryAuthority::open(&selected_parent).unwrap();

        assert!(require_source_in_parent(&clip, selected_identity, &authority).is_err());
        assert_eq!(std::fs::read(&foreign_poster).unwrap(), b"foreign poster");
        assert_eq!(std::fs::read_dir(&selected_parent).unwrap().count(), 2);

        drop(authority);
        std::fs::remove_dir_all(container).unwrap();
    }

    #[test]
    fn poster_child_concurrently_drains_bounded_stdout_and_stderr() {
        let mut command = Command::new(std::env::current_exe().expect("locate test executable"));
        command
            .args([
                "--ignored",
                "--exact",
                "poster::tests::poster_pipe_fixture_fills_both_streams",
                "--nocapture",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let output = run_poster_child_with_limits(&mut command, Duration::from_secs(10), 1024, 512)
            .expect("both full child pipes must be drained without deadlock");
        assert!(output.status.success());
        assert_eq!(output.stdout.bytes.len(), 1024);
        assert_eq!(output.stderr.bytes.len(), 512);
        assert!(output.stdout.overflowed);
        assert!(output.stderr.overflowed);
    }

    #[test]
    fn poster_child_enforces_production_stdout_and_stderr_limits() {
        let emitted = MAX_POSTER_ENCODED_BYTES + 8 * 1024;
        let mut command = Command::new(std::env::current_exe().expect("locate test executable"));
        command
            .args([
                "--ignored",
                "--exact",
                "poster::tests::poster_pipe_fixture_fills_both_streams",
                "--nocapture",
            ])
            .env("CLIPLINE_POSTER_FIXTURE_BYTES", emitted.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let output = run_poster_child(&mut command, Duration::from_secs(15))
            .expect("production limits must drain the child without deadlock");
        assert!(output.status.success());
        assert_eq!(output.stdout.bytes.len(), MAX_POSTER_ENCODED_BYTES);
        assert!(output.stdout.overflowed);
        assert_eq!(output.stderr.bytes.len(), MAX_POSTER_STDERR_BYTES);
        assert!(output.stderr.overflowed);
    }

    #[test]
    fn stdout_reader_spawn_failure_kills_and_reaps_the_child() {
        let mut command = Command::new(std::env::current_exe().expect("locate test executable"));
        command
            .args([
                "--ignored",
                "--exact",
                "poster::tests::poster_timeout_fixture_sleeps",
                "--nocapture",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let started = Instant::now();

        let error = match run_poster_child_with_reader_spawner(
            &mut command,
            Duration::from_secs(10),
            1024,
            512,
            |name, reader, maximum| {
                if name == "clipline-poster-stdout" {
                    return Err(io::Error::other("injected reader spawn failure"));
                }
                spawn_bounded_reader(name, reader, maximum)
            },
        ) {
            Ok(_) => panic!("injected stdout reader spawn failure must fail"),
            Err(error) => error,
        };

        assert!(error.contains("stdout reader"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn full_poster_child_timeout_path_kills_joins_and_returns() {
        let mut command = Command::new(std::env::current_exe().expect("locate test executable"));
        command
            .args([
                "--ignored",
                "--exact",
                "poster::tests::poster_timeout_fixture_sleeps",
                "--nocapture",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let error = match run_poster_child_with_limits(
            &mut command,
            Duration::from_millis(50),
            1024,
            512,
        ) {
            Ok(_) => panic!("timeout fixture must time out"),
            Err(error) => error,
        };

        assert!(error.contains("timed out"));
    }

    #[test]
    fn owned_temps_are_unique_and_drop_cleans_each_exact_file() {
        let directory = test_directory("owned-temp");
        let poster = directory.join("clip.poster.jpg");
        let authority = clipline_shell::DirectoryAuthority::open(&directory).unwrap();
        let first = PosterTemp::reserve(&poster, &authority).unwrap();
        let second = PosterTemp::reserve(&poster, &authority).unwrap();
        let first_path = first.path();
        let second_path = second.path();

        assert_ne!(first_path, second_path);
        drop(first);
        assert!(!first_path.exists());
        assert!(second_path.exists());
        drop(second);
        assert!(!second_path.exists());
        drop(authority);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn temp_identity_failure_retries_identity_and_cleans_the_exact_reservation() {
        let directory = test_directory("temp-identity-failure");
        let authority = clipline_shell::DirectoryAuthority::open(&directory).unwrap();
        let name = OsString::from("clip.poster.jpg.tmp.injected");
        let path = directory.join(&name);
        let file = authority.create_new_regular_file(&name).unwrap();

        let error = identify_reserved_temp_with(file, &authority, &name, |_| {
            Err(io::Error::other("injected identity failure"))
        })
        .unwrap_err();

        assert!(error.contains("identify poster temp"));
        assert!(!path.exists());
        drop(authority);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn atomic_publish_replaces_only_the_target_selected_at_reservation() {
        let directory = test_directory("atomic-target-fence");
        let poster = directory.join("clip.poster.jpg");
        std::fs::write(&poster, b"stale").unwrap();
        let authority = clipline_shell::DirectoryAuthority::open(&directory).unwrap();
        let mut temporary = PosterTemp::reserve(&poster, &authority).unwrap();
        let temporary_path = temporary.path();
        temporary.write_all(b"\xff\xd8complete\xff\xd9").unwrap();

        std::fs::remove_file(&poster).unwrap();
        std::fs::write(&poster, b"concurrent winner").unwrap();
        assert!(temporary.publish(&poster).is_err());
        assert_eq!(std::fs::read(&poster).unwrap(), b"concurrent winner");
        assert!(!temporary_path.exists());
        drop(authority);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn create_new_publish_never_replaces_a_concurrent_winner() {
        let directory = test_directory("atomic-create-fence");
        let poster = directory.join("clip.poster.jpg");
        let authority = clipline_shell::DirectoryAuthority::open(&directory).unwrap();
        let mut temporary = PosterTemp::reserve(&poster, &authority).unwrap();
        let temporary_path = temporary.path();
        temporary.write_all(b"\xff\xd8complete\xff\xd9").unwrap();
        std::fs::write(&poster, b"concurrent winner").unwrap();

        assert!(temporary.publish(&poster).is_err());
        assert_eq!(std::fs::read(&poster).unwrap(), b"concurrent winner");
        assert!(!temporary_path.exists());
        drop(authority);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn empty_overflowed_or_corrupt_output_never_replaces_a_stale_poster() {
        let directory = test_directory("invalid-output");
        let poster = directory.join("clip.poster.jpg");
        std::fs::write(&poster, b"stale").unwrap();
        let authority = clipline_shell::DirectoryAuthority::open(&directory).unwrap();

        for output in [
            BoundedPipeOutput {
                bytes: Vec::new(),
                overflowed: false,
            },
            BoundedPipeOutput {
                bytes: vec![0xff, 0xd8, 0xff, 0xd9],
                overflowed: true,
            },
            BoundedPipeOutput {
                bytes: b"not a jpeg".to_vec(),
                overflowed: false,
            },
            BoundedPipeOutput {
                bytes: b"\xff\xd8corrupt entropy\xff\xd9".to_vec(),
                overflowed: false,
            },
            BoundedPipeOutput {
                bytes: vec![0xff, 0xd8, 0x00, 0x00],
                overflowed: false,
            },
        ] {
            assert!(publish_poster_output(&poster, output, &authority).is_err());
            assert_eq!(std::fs::read(&poster).unwrap(), b"stale");
        }
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        drop(authority);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn injected_ffmpeg_pipeline_rejects_corrupt_output_then_publishes_valid_jpeg() {
        let directory = test_directory("injected-ffmpeg-pipeline");
        let clip = directory.join("clip.mp4");
        let payload = directory.join("poster-payload.bin");
        std::fs::write(&clip, b"clip").unwrap();
        std::fs::write(&payload, b"not a jpeg").unwrap();
        let fake_ffmpeg = fake_ffmpeg_script(&directory, &payload);
        let located = fake_ffmpeg.clone();
        let extractor = FfmpegPosterExtractor::new(Arc::new(move || Some(located.clone())));
        let canonical = clip.canonicalize().unwrap();

        let error = extractor.extract(&canonical, 1.0).unwrap_err();
        assert!(error.contains("JPEG"));
        assert!(!poster_path(&canonical).exists());
        assert!(std::fs::read_dir(&directory).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".tmp.")));

        let jpeg = test_jpeg();
        std::fs::write(&payload, &jpeg).unwrap();
        let poster = extractor.extract(&canonical, 1.0).unwrap();
        assert_eq!(std::fs::read(poster).unwrap(), jpeg);
        assert_eq!(extractor.successful_path.get(), Some(&fake_ffmpeg));

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn poster_publication_stays_with_a_renamed_selected_parent() {
        let container = test_directory("poster-parent-authority");
        let selected = container.join("selected");
        let moved = container.join("moved");
        std::fs::create_dir(&selected).unwrap();
        let poster = selected.join("clip.poster.jpg");
        std::fs::write(&poster, b"selected stale poster").unwrap();
        let authority = clipline_shell::DirectoryAuthority::open(&selected).unwrap();
        let mut temporary = PosterTemp::reserve(&poster, &authority).unwrap();
        let jpeg = test_jpeg();
        temporary.write_all(&jpeg).unwrap();

        std::fs::rename(&selected, &moved).unwrap();
        std::fs::create_dir(&selected).unwrap();
        let foreign_poster = selected.join("clip.poster.jpg");
        std::fs::write(&foreign_poster, b"foreign poster").unwrap();

        temporary.publish(&poster).unwrap();

        assert_eq!(std::fs::read(&foreign_poster).unwrap(), b"foreign poster");
        assert_eq!(std::fs::read(moved.join("clip.poster.jpg")).unwrap(), jpeg);
        assert_eq!(std::fs::read_dir(&selected).unwrap().count(), 1);
        drop(authority);
        std::fs::remove_dir_all(container).unwrap();
    }

    #[test]
    fn timed_out_poster_child_is_killed_and_reaped() {
        let mut child = Command::new(std::env::current_exe().expect("locate test executable"))
            .args([
                "--ignored",
                "--exact",
                "poster::tests::poster_timeout_fixture_sleeps",
                "--nocapture",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn timeout fixture");

        let wait = wait_for_poster_child(&mut child, Duration::from_millis(25)).unwrap();
        assert!(matches!(wait, PosterChildWait::TimedOut));
        assert!(child.try_wait().unwrap().is_some());
    }

    #[test]
    #[ignore = "subprocess-only timeout fixture"]
    fn poster_timeout_fixture_sleeps() {
        std::thread::sleep(Duration::from_secs(30));
    }

    #[test]
    #[ignore = "subprocess-only pipe fixture"]
    fn poster_pipe_fixture_fills_both_streams() {
        let emitted = std::env::var("CLIPLINE_POSTER_FIXTURE_BYTES")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(256 * 1024);
        let bytes = vec![b'x'; emitted];
        let mut stdout = io::stdout().lock();
        stdout.write_all(&bytes).unwrap();
        stdout.flush().unwrap();
        drop(stdout);
        let mut stderr = io::stderr().lock();
        stderr.write_all(&bytes).unwrap();
        stderr.flush().unwrap();
    }

    #[cfg(any(unix, windows))]
    fn fake_ffmpeg_script(directory: &Path, payload: &Path) -> PathBuf {
        #[cfg(windows)]
        {
            let script = directory.join("fake-ffmpeg.cmd");
            let payload = payload.to_string_lossy().replace('\'', "''");
            let body = format!(
                "@echo off\r\npowershell.exe -NoProfile -NonInteractive -Command \"$b=[IO.File]::ReadAllBytes('{payload}');$o=[Console]::OpenStandardOutput();$o.Write($b,0,$b.Length)\"\r\n"
            );
            std::fs::write(&script, body).unwrap();
            script
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let script = directory.join("fake-ffmpeg");
            let payload = payload.to_string_lossy().replace('\'', "'\\''");
            std::fs::write(&script, format!("#!/bin/sh\nexec cat -- '{payload}'\n")).unwrap();
            let mut permissions = std::fs::metadata(&script).unwrap().permissions();
            permissions.set_mode(0o700);
            std::fs::set_permissions(&script, permissions).unwrap();
            script
        }
    }

    fn test_directory(case: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "clipline-poster-{case}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&directory).unwrap();
        directory
    }
}
