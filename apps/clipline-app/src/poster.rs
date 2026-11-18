//! Library poster frames: a cached JPEG thumbnail beside each clip
//! (`<clip>.poster.jpg`), extracted with ffmpeg at a representative moment
//! (chosen by the caller — typically the first event marker). The gallery
//! loads these through the asset protocol, the same path clips play back
//! through, so no new scope is needed.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use clipline_capture::app_control::AppControlBlock;
use clipline_capture::ffmpeg::{Discovery, FfmpegUnavailable};

use crate::library::suppress_console;

/// Poster width in pixels. Cards render ~250px wide; 480 covers 2x displays
/// while keeping each JPEG to a few tens of KB. Height follows the aspect
/// ratio (`-2` keeps it even for the encoder).
const POSTER_WIDTH: u32 = 480;
const POSTER_EXTRACTION_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_POSTER_STDOUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_POSTER_STDERR_BYTES: usize = 64 * 1024;
static NEXT_POSTER_TEMP_ID: AtomicU64 = AtomicU64::new(0);
static POSTER_FFMPEG: OnceLock<PathBuf> = OnceLock::new();

/// The cached poster path for a clip: `clip.mp4` -> `clip.poster.jpg`. Mirrors
/// the `<clip>.markers.json` sidecar convention so the two travel together.
pub fn poster_path(clip: &Path) -> PathBuf {
    clipline_storage::clip_sidecar_path(clip, clipline_storage::POSTER_SUFFIX)
}

/// Return the ready cache entry without starting ffmpeg. This keeps gallery
/// cache hits out of the bounded extraction queue.
pub fn cached_poster(clip: &Path) -> Option<PathBuf> {
    let poster = poster_path(clip);
    poster_is_fresh(clip, &poster).then_some(poster)
}

/// Return the cached poster for `clip`, generating it with ffmpeg if missing or
/// stale. `seek_s` is the timestamp to grab the frame from (clamped to >= 0).
/// A fresh cache hit never touches ffmpeg; an error means generation was
/// attempted and failed (e.g. no ffmpeg, unreadable clip).
pub fn ensure_poster(clip: &Path, seek_s: f64) -> Result<PathBuf, String> {
    if let Some(poster) = cached_poster(clip) {
        return Ok(poster);
    }
    let poster = poster_path(clip);
    extract_unless_recently_blocked(
        &POSTER_FFMPEG_BLOCKED,
        Instant::now(),
        &POSTER_FFMPEG,
        clipline_capture::ffmpeg::discover,
        |ffmpeg| generate_poster(ffmpeg, clip, &poster, seek_s),
    )
    .map_err(|failure| failure.to_string())?;
    Ok(poster)
}

/// When Windows blocks FFmpeg, each gallery card would otherwise rehash the
/// runtime and spawn a child Windows refuses. Reuse the verdict for a while
/// instead. Allowing FFmpeg takes effect within one interval.
const BLOCKED_RECHECK_INTERVAL: Duration = Duration::from_secs(60);
static POSTER_FFMPEG_BLOCKED: Mutex<Option<(Instant, AppControlBlock)>> = Mutex::new(None);

/// Why one poster extraction failed. A refusal by Windows Application Control
/// stays typed so it can pause later spawns.
#[derive(Debug, PartialEq, Eq)]
enum PosterFailure {
    Blocked(AppControlBlock),
    Failed(String),
}

impl From<String> for PosterFailure {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

impl From<FfmpegUnavailable> for PosterFailure {
    fn from(unavailable: FfmpegUnavailable) -> Self {
        match unavailable {
            FfmpegUnavailable::Blocked(block) => Self::Blocked(block),
            FfmpegUnavailable::Missing(message) => Self::Failed(message),
        }
    }
}

impl std::fmt::Display for PosterFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Blocked(block) => block.fmt(f),
            Self::Failed(message) => f.write_str(message),
        }
    }
}

/// Locate FFmpeg and run one poster extraction, unless Windows refused FFmpeg
/// within the last recheck interval. The pause covers the cached path too, and
/// a refusal from discovery or from the poster child itself starts it. The
/// latch is not held while the child runs, so extractions stay concurrent.
fn extract_unless_recently_blocked(
    latch: &Mutex<Option<(Instant, AppControlBlock)>>,
    now: Instant,
    cache: &OnceLock<PathBuf>,
    discover: impl FnOnce() -> Discovery,
    generate: impl FnOnce(&Path) -> Result<(), PosterFailure>,
) -> Result<(), PosterFailure> {
    let lock = || latch.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((seen, block)) = *lock() {
        if now.saturating_duration_since(seen) < BLOCKED_RECHECK_INTERVAL {
            return Err(PosterFailure::Blocked(block));
        }
    }
    let result = cached_successful_path(cache, || {
        discover().require("ffmpeg is not available for poster extraction")
    })
    .map_err(PosterFailure::from)
    .and_then(|ffmpeg| generate(&ffmpeg));
    match &result {
        Err(PosterFailure::Blocked(block)) => *lock() = Some((now, *block)),
        Ok(()) => *lock() = None,
        Err(PosterFailure::Failed(_)) => {}
    }
    result
}

/// Cache only a successful lookup. A missing executable may be installed or
/// configured, and a blocked one allowed, while Clipline is running, so
/// failures must remain retryable.
fn cached_successful_path<E>(
    cache: &OnceLock<PathBuf>,
    locate: impl FnOnce() -> Result<PathBuf, E>,
) -> Result<PathBuf, E> {
    if let Some(path) = cache.get() {
        return Ok(path.clone());
    }
    let located = locate()?;
    Ok(cache.get_or_init(|| located).clone())
}

/// A poster is fresh when it exists and is at least as new as its clip, so a
/// clip replaced at the same path regenerates its thumbnail.
fn poster_is_fresh(clip: &Path, poster: &Path) -> bool {
    let Ok(poster_modified) = std::fs::metadata(poster).and_then(|m| m.modified()) else {
        return false;
    };
    match std::fs::metadata(clip).and_then(|m| m.modified()) {
        Ok(clip_modified) => poster_modified >= clip_modified,
        // Can't read the clip's mtime — trust the existing poster rather than
        // churn ffmpeg on every listing.
        Err(_) => true,
    }
}

fn generate_poster(
    ffmpeg: &Path,
    clip: &Path,
    poster: &Path,
    seek_s: f64,
) -> Result<(), PosterFailure> {
    let mut cmd = poster_command(ffmpeg, clip, seek_s);
    let output = run_poster_child(&mut cmd, POSTER_EXTRACTION_TIMEOUT)?;
    publish_poster_output(poster, poster_stdout(output)?).map_err(PosterFailure::from)
}

fn poster_stdout(output: PosterChildOutput) -> Result<BoundedPipeOutput, PosterFailure> {
    if let Some(block) = clipline_capture::app_control::block_from_exit_status(&output.status) {
        return Err(PosterFailure::Blocked(block));
    }
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr.bytes);
        return Err(format!("ffmpeg poster failed: {stderr}").into());
    }
    Ok(output.stdout)
}

fn publish_poster_output(poster: &Path, stdout: BoundedPipeOutput) -> Result<(), String> {
    if stdout.overflowed {
        return Err(format!(
            "ffmpeg poster exceeded the {MAX_POSTER_STDOUT_BYTES}-byte output limit"
        ));
    }
    if stdout.bytes.is_empty() {
        return Err("ffmpeg poster produced no JPEG data".to_string());
    }

    // FFmpeg only reads the clip and emits the JPEG through stdout. Clipline
    // owns the sibling write, so Windows Controlled Folder Access does not
    // need to grant the independently distributed ffmpeg child write access
    // to the user's media folder. Publishing through a sibling temp keeps the
    // existing crash-safe atomic replacement.
    let mut tmp = PosterTemp::reserve(poster)?;
    tmp.write_all(&stdout.bytes)?;
    tmp.publish(poster)
}

fn poster_command(ffmpeg: &Path, clip: &Path, seek_s: f64) -> Command {
    let mut cmd = Command::new(ffmpeg);
    suppress_console(&mut cmd);
    // Input-side `-ss` is a fast keyframe seek — fine for a thumbnail.
    cmd.args(["-hide_banner", "-nostdin", "-ss", &seek_arg(seek_s), "-i"])
        .arg(clip)
        .args([
            "-frames:v",
            "1",
            "-vf",
            &scale_filter(),
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
    cmd
}

#[derive(Debug)]
enum PosterChildWait {
    Exited(ExitStatus),
    TimedOut,
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

/// Wait without leaving an ffmpeg child behind. Timeout and `try_wait`
/// failures both kill and reap before returning.
fn wait_for_poster_child(child: &mut Child, timeout: Duration) -> io::Result<PosterChildWait> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(PosterChildWait::Exited(status)),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(PosterChildWait::TimedOut);
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        }
    }
}

fn run_poster_child(
    command: &mut Command,
    timeout: Duration,
) -> Result<PosterChildOutput, PosterFailure> {
    run_poster_child_with_limits(
        command,
        timeout,
        MAX_POSTER_STDOUT_BYTES,
        MAX_POSTER_STDERR_BYTES,
    )
}

fn run_poster_child_with_limits(
    command: &mut Command,
    timeout: Duration,
    max_stdout_bytes: usize,
    max_stderr_bytes: usize,
) -> Result<PosterChildOutput, PosterFailure> {
    let mut child = clipline_capture::ffmpeg::spawn_verified(command).map_err(|error| {
        match clipline_capture::app_control::block_from_io_error(&error) {
            Some(block) => PosterFailure::Blocked(block),
            None => format!("spawn ffmpeg poster: {error}").into(),
        }
    })?;
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("spawn ffmpeg poster: stdout or stderr pipe unavailable"
            .to_string()
            .into());
    };
    let stdout_reader =
        match spawn_bounded_reader("clipline-poster-stdout", stdout, max_stdout_bytes) {
            Ok(reader) => reader,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("spawn ffmpeg poster stdout reader: {error}").into());
            }
        };
    let stderr_reader =
        match spawn_bounded_reader("clipline-poster-stderr", stderr, max_stderr_bytes) {
            Ok(reader) => reader,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                return Err(format!("spawn ffmpeg poster stderr reader: {error}").into());
            }
        };

    let wait = wait_for_poster_child(&mut child, timeout);
    // Both readers are running while the child is alive. Join both before
    // propagating either result so an error on one pipe cannot detach the
    // other reader.
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
        )
        .into()),
    }
}

fn spawn_bounded_reader(
    name: &str,
    reader: impl Read + Send + 'static,
    max_bytes: usize,
) -> io::Result<std::thread::JoinHandle<io::Result<BoundedPipeOutput>>> {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || read_bounded(reader, max_bytes))
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

/// Retain at most `max_bytes` while continuing to drain the pipe to EOF. The
/// drain is essential: stopping at the memory limit could fill the child's
/// pipe and deadlock it before timeout handling can observe completion.
fn read_bounded(mut reader: impl Read, max_bytes: usize) -> io::Result<BoundedPipeOutput> {
    let mut retained = Vec::new();
    let mut overflowed = false;
    let mut chunk = [0u8; 8 * 1024];
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            return Ok(BoundedPipeOutput {
                bytes: retained,
                overflowed,
            });
        }
        let keep = read.min(max_bytes.saturating_sub(retained.len()));
        retained.extend_from_slice(&chunk[..keep]);
        overflowed |= keep < read;
    }
}

struct PosterTemp {
    path: PathBuf,
    file: Option<File>,
    armed: bool,
}

impl PosterTemp {
    fn reserve(poster: &Path) -> Result<Self, String> {
        let file_name = poster
            .file_name()
            .ok_or_else(|| "poster path has no file name".to_string())?;
        for _ in 0..64 {
            let id = NEXT_POSTER_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let mut temp_name = file_name.to_os_string();
            temp_name.push(format!(".tmp.{}.{id}", std::process::id()));
            let path = poster.with_file_name(temp_name);
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file: Some(file),
                        armed: true,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(format!("reserve poster temp: {error}")),
            }
        }
        Err("reserve poster temp: unique-name attempts exhausted".to_string())
    }

    #[cfg(test)]
    fn path(&self) -> &Path {
        &self.path
    }

    fn write_all(&mut self, jpeg: &[u8]) -> Result<(), String> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| "poster temp is already closed".to_string())?;
        file.write_all(jpeg)
            .and_then(|()| file.flush())
            .map_err(|error| format!("write poster temp: {error}"))
    }

    fn publish(mut self, poster: &Path) -> Result<(), String> {
        // Close the handle before the atomic rename; publishing must not
        // depend on the platform's sharing flags for an open Rust File.
        drop(self.file.take());
        atomic_replace_file(&self.path, poster)
            .map_err(|error| format!("finalize poster: {error}"))?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for PosterTemp {
    fn drop(&mut self) {
        drop(self.file.take());
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(windows)]
fn atomic_replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    crate::windows::replace_file(from, to)
}

#[cfg(not(windows))]
fn atomic_replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)
}

/// `-ss` value: seconds with millisecond precision, never negative.
fn seek_arg(seek_s: f64) -> String {
    format!("{:.3}", seek_s.max(0.0))
}

fn scale_filter() -> String {
    format!("scale={POSTER_WIDTH}:-2:flags=bicubic")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn poster_path_swaps_mp4_for_poster_jpg() {
        assert_eq!(
            poster_path(Path::new(r"C:\clips\2026\session_1.mp4")),
            PathBuf::from(r"C:\clips\2026\session_1.poster.jpg")
        );
    }

    #[test]
    fn ffmpeg_cache_retries_misses_and_keeps_the_first_success() {
        let cache = OnceLock::new();
        let attempts = Cell::new(0);

        assert_eq!(
            cached_successful_path(&cache, || {
                attempts.set(attempts.get() + 1);
                Err("missing")
            }),
            Err("missing")
        );
        assert!(
            cache.get().is_none(),
            "a missing or blocked executable must not initialize the cache"
        );

        let expected = PathBuf::from(r"C:\tools\ffmpeg.exe");
        assert_eq!(
            cached_successful_path(&cache, || {
                attempts.set(attempts.get() + 1);
                Ok::<_, &str>(expected.clone())
            }),
            Ok(expected.clone())
        );
        assert_eq!(
            cached_successful_path(&cache, || {
                attempts.set(attempts.get() + 1);
                Ok::<_, &str>(PathBuf::from(r"C:\other\ffmpeg.exe"))
            }),
            Ok(expected)
        );
        assert_eq!(attempts.get(), 2, "a success must remain cached");
    }

    fn smart_app_control_block() -> AppControlBlock {
        clipline_capture::app_control::block_from_exit_code(Some(0xC0E9_0002_u32 as i32)).unwrap()
    }

    #[test]
    fn a_runtime_blocked_at_discovery_is_rechecked_at_most_once_per_window() {
        let block = smart_app_control_block();
        let ffmpeg = PathBuf::from(r"C:\ffmpeg\ffmpeg.exe");
        let latch = Mutex::new(None);
        let cache = OnceLock::new();
        let discoveries = Cell::new(0);
        let spawns = Cell::new(0);
        let blocked = || {
            discoveries.set(discoveries.get() + 1);
            Discovery::Blocked {
                path: ffmpeg.clone(),
                block,
            }
        };
        let spawn = |_: &Path| {
            spawns.set(spawns.get() + 1);
            Ok(())
        };
        let start = Instant::now();

        let first = extract_unless_recently_blocked(&latch, start, &cache, blocked, spawn);
        let soon = extract_unless_recently_blocked(
            &latch,
            start + BLOCKED_RECHECK_INTERVAL / 2,
            &cache,
            blocked,
            spawn,
        );
        assert_eq!(first, Err(PosterFailure::Blocked(block)));
        assert_eq!(soon, Err(PosterFailure::Blocked(block)));
        assert_eq!(discoveries.get(), 1, "no re-spawn inside the window");
        assert_eq!(spawns.get(), 0);

        let later = extract_unless_recently_blocked(
            &latch,
            start + BLOCKED_RECHECK_INTERVAL,
            &cache,
            || {
                discoveries.set(discoveries.get() + 1);
                Discovery::Found(ffmpeg.clone())
            },
            spawn,
        );
        assert_eq!(later, Ok(()));
        assert_eq!(discoveries.get(), 2, "the block is rechecked after the window");
        assert_eq!(spawns.get(), 1);
        assert!(latch.lock().unwrap().is_none(), "an allowed runtime clears the latch");
    }

    #[test]
    fn a_blocked_poster_child_pauses_later_spawns_even_with_a_cached_path() {
        let block = smart_app_control_block();
        let latch = Mutex::new(None);
        let cache = OnceLock::from(PathBuf::from(r"C:\ffmpeg\ffmpeg.exe"));
        let spawns = Cell::new(0);
        let refused = |_: &Path| {
            spawns.set(spawns.get() + 1);
            Err(PosterFailure::Blocked(block))
        };
        let no_discovery = || -> Discovery { panic!("a cached path needs no discovery") };
        let start = Instant::now();

        let first = extract_unless_recently_blocked(&latch, start, &cache, no_discovery, refused);
        let soon = extract_unless_recently_blocked(
            &latch,
            start + BLOCKED_RECHECK_INTERVAL / 2,
            &cache,
            no_discovery,
            refused,
        );
        assert_eq!(first, Err(PosterFailure::Blocked(block)));
        assert_eq!(soon, Err(PosterFailure::Blocked(block)));
        assert_eq!(spawns.get(), 1, "a refused child must pause the next thumbnails");

        let later = extract_unless_recently_blocked(
            &latch,
            start + BLOCKED_RECHECK_INTERVAL,
            &cache,
            no_discovery,
            refused,
        );
        assert_eq!(later, Err(PosterFailure::Blocked(block)));
        assert_eq!(spawns.get(), 2, "the block is rechecked once the window passes");
    }

    #[test]
    fn a_recent_block_stops_a_cached_path_from_spawning() {
        let block = smart_app_control_block();
        let start = Instant::now();
        let latch = Mutex::new(Some((start, block)));
        let cache = OnceLock::from(PathBuf::from(r"C:\ffmpeg\ffmpeg.exe"));

        let result = extract_unless_recently_blocked(
            &latch,
            start + Duration::from_secs(1),
            &cache,
            || -> Discovery { panic!("a recent block skips discovery") },
            |_: &Path| -> Result<(), PosterFailure> { panic!("a recent block skips the spawn") },
        );

        assert_eq!(result, Err(PosterFailure::Blocked(block)));
    }

    #[test]
    fn ordinary_poster_failures_do_not_pause_spawns() {
        let latch = Mutex::new(None);
        let cache = OnceLock::from(PathBuf::from(r"C:\ffmpeg\ffmpeg.exe"));
        let failed = |_: &Path| Err(PosterFailure::Failed("ffmpeg poster failed: bad clip".into()));
        let start = Instant::now();

        let _ = extract_unless_recently_blocked(&latch, start, &cache, || Discovery::Missing, failed);

        assert!(latch.lock().unwrap().is_none(), "only an App Control refusal pauses spawns");
    }

    #[test]
    fn seek_arg_clamps_negative_and_keeps_millisecond_precision() {
        assert_eq!(seek_arg(12.5), "12.500");
        assert_eq!(seek_arg(0.0), "0.000");
        assert_eq!(seek_arg(-3.0), "0.000");
    }

    #[test]
    fn poster_command_streams_one_mjpeg_to_stdout() {
        let clip = Path::new(r"C:\clips\clip.mp4");
        let command = poster_command(Path::new(r"C:\tools\ffmpeg.exe"), clip, 2.5);
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(args
            .windows(2)
            .any(|pair| pair[0] == "-c:v" && pair[1] == "mjpeg"));
        assert!(args
            .windows(2)
            .any(|pair| pair[0] == "-f" && pair[1] == "image2pipe"));
        assert_eq!(args.last().map(String::as_str), Some("pipe:1"));
        assert!(args
            .iter()
            .any(|arg| arg == clip.to_string_lossy().as_ref()));
        assert!(
            !args.iter().any(|arg| arg.ends_with(".poster.jpg")),
            "ffmpeg must never receive a path beside the user's clip"
        );
    }

    #[test]
    fn bounded_reader_keeps_the_prefix_and_drains_overflow() {
        let output = read_bounded(std::io::Cursor::new(b"abcdefgh"), 3).unwrap();

        assert_eq!(output.bytes, b"abc");
        assert!(output.overflowed);
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
    fn poster_is_stale_when_missing() {
        let dir = std::env::temp_dir().join(format!("clipline-poster-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("clip.mp4");
        std::fs::write(&clip, b"\0\0\0\0").unwrap();
        assert!(!poster_is_fresh(&clip, &poster_path(&clip)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_poster_temps_have_independent_owned_paths() {
        let dir = test_dir("owned-temp");
        let poster = dir.join("clip.poster.jpg");
        let first = PosterTemp::reserve(&poster).unwrap();
        let second = PosterTemp::reserve(&poster).unwrap();
        let first_path = first.path().to_path_buf();
        let second_path = second.path().to_path_buf();

        assert_ne!(first_path, second_path);
        assert_eq!(first_path.parent(), poster.parent());
        assert_eq!(second_path.parent(), poster.parent());
        assert!(first_path.exists());
        assert!(second_path.exists());

        drop(first);
        assert!(!first_path.exists());
        assert!(second_path.exists());
        drop(second);
        assert!(!second_path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn poster_temp_atomically_replaces_stale_destination() {
        let dir = test_dir("atomic-publish");
        let poster = dir.join("clip.poster.jpg");
        std::fs::write(&poster, b"stale").unwrap();
        let mut temp = PosterTemp::reserve(&poster).unwrap();
        let temp_path = temp.path().to_path_buf();
        temp.write_all(b"complete jpeg").unwrap();

        temp.publish(&poster).unwrap();

        assert_eq!(std::fs::read(&poster).unwrap(), b"complete jpeg");
        assert!(!temp_path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn invalid_pipe_output_never_replaces_a_stale_poster() {
        let dir = test_dir("invalid-output");
        let poster = dir.join("clip.poster.jpg");
        std::fs::write(&poster, b"stale").unwrap();

        let empty = publish_poster_output(
            &poster,
            BoundedPipeOutput {
                bytes: Vec::new(),
                overflowed: false,
            },
        )
        .unwrap_err();
        let overflow = publish_poster_output(
            &poster,
            BoundedPipeOutput {
                bytes: vec![0xff, 0xd8],
                overflowed: true,
            },
        )
        .unwrap_err();

        assert!(empty.contains("no JPEG data"));
        assert!(overflow.contains("output limit"));
        assert_eq!(std::fs::read(&poster).unwrap(), b"stale");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(dir);
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

        let wait = wait_for_poster_child(&mut child, std::time::Duration::from_millis(25))
            .expect("wait for timeout fixture");

        assert!(matches!(wait, PosterChildWait::TimedOut));
        assert!(
            child.try_wait().unwrap().is_some(),
            "timed-out poster child must be reaped"
        );
    }

    #[test]
    fn blocked_poster_child_reports_the_app_control_block() {
        let mut command = Command::new(std::env::current_exe().expect("locate test executable"));
        command
            .args([
                "--ignored",
                "--exact",
                "poster::tests::poster_blocked_fixture_exits",
                "--nocapture",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = run_poster_child_with_limits(&mut command, Duration::from_secs(10), 1024, 512)
            .expect("run blocked fixture");

        let Err(failure) = poster_stdout(output) else {
            panic!("a blocked child produces no poster");
        };
        assert!(
            matches!(failure, PosterFailure::Blocked(_)),
            "the refusal must stay typed so it can pause later spawns: {failure:?}"
        );
        let error = failure.to_string();

        assert!(
            clipline_capture::app_control::is_blocked_message(&error),
            "{error}"
        );
        assert!(!error.starts_with("ffmpeg poster failed"), "{error}");
        assert_eq!(
            crate::library::poster_failure_kind(&error),
            "blocked_by_app_control"
        );
    }

    #[test]
    #[ignore = "subprocess-only App Control fixture"]
    fn poster_blocked_fixture_exits() {
        // ffmpeg's exit code when Smart App Control refuses one of its DLLs.
        std::process::exit(0xC0E9_0002_u32 as i32);
    }

    #[test]
    #[ignore = "subprocess-only timeout fixture"]
    fn poster_timeout_fixture_sleeps() {
        std::thread::sleep(std::time::Duration::from_secs(30));
    }

    #[test]
    #[ignore = "subprocess-only pipe fixture"]
    fn poster_pipe_fixture_fills_both_streams() {
        let bytes = vec![b'x'; 256 * 1024];
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(&bytes).unwrap();
        stdout.flush().unwrap();
        drop(stdout);

        let mut stderr = std::io::stderr().lock();
        stderr.write_all(&bytes).unwrap();
        stderr.flush().unwrap();
    }

    fn test_dir(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "clipline-poster-{label}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
