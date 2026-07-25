//! Library poster frames: a cached JPEG thumbnail beside each clip
//! (`<clip>.poster.jpg`), extracted with ffmpeg at a representative moment
//! (chosen by the caller — typically the first event marker). The gallery
//! loads these through the asset protocol, the same path clips play back
//! through, so no new scope is needed.

use std::fs::OpenOptions;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::library::suppress_console;

/// Poster width in pixels. Cards render ~250px wide; 480 covers 2x displays
/// while keeping each JPEG to a few tens of KB. Height follows the aspect
/// ratio (`-2` keeps it even for the encoder).
const POSTER_WIDTH: u32 = 480;
const POSTER_EXTRACTION_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_POSTER_STDERR_BYTES: usize = 64 * 1024;
static NEXT_POSTER_TEMP_ID: AtomicU64 = AtomicU64::new(0);
static POSTER_FFMPEG: OnceLock<PathBuf> = OnceLock::new();

/// The cached poster path for a clip: `clip.mp4` -> `clip.poster.jpg`. Mirrors
/// the `<clip>.markers.json` sidecar convention so the two travel together.
pub fn poster_path(clip: &Path) -> PathBuf {
    clip.with_extension("poster.jpg")
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
    let ffmpeg = cached_successful_path(&POSTER_FFMPEG, clipline_capture::ffmpeg::locate)
        .ok_or_else(|| "ffmpeg is not available for poster extraction".to_string())?;
    generate_poster(&ffmpeg, clip, &poster, seek_s)?;
    Ok(poster)
}

/// Cache only a successful lookup. A missing executable may be installed or
/// configured while Clipline is running, so `None` must remain retryable.
fn cached_successful_path(
    cache: &OnceLock<PathBuf>,
    locate: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(path) = cache.get() {
        return Some(path.clone());
    }
    let located = locate()?;
    let _ = cache.set(located);
    cache.get().cloned()
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

fn generate_poster(ffmpeg: &Path, clip: &Path, poster: &Path, seek_s: f64) -> Result<(), String> {
    // Write to a sibling temp then rename, so a crash mid-encode never leaves a
    // half-written poster the gallery would cache.
    let tmp = PosterTemp::reserve(poster)?;

    let mut cmd = Command::new(ffmpeg);
    suppress_console(&mut cmd);
    // Input-side `-ss` is a fast keyframe seek — fine for a thumbnail.
    cmd.args([
        "-hide_banner",
        "-nostdin",
        "-y",
        "-ss",
        &seek_arg(seek_s),
        "-i",
    ])
    .arg(clip)
    .args([
        "-frames:v",
        "1",
        "-vf",
        &scale_filter(),
        "-q:v",
        "4",
        "-f",
        "image2",
    ])
    .arg(tmp.path())
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::piped());
    let (status, stderr) = run_poster_child(&mut cmd, POSTER_EXTRACTION_TIMEOUT)?;
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr);
        return Err(format!("ffmpeg poster failed: {stderr}"));
    }
    tmp.publish(poster)
}

#[derive(Debug)]
enum PosterChildWait {
    Exited(ExitStatus),
    TimedOut,
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
) -> Result<(ExitStatus, Vec<u8>), String> {
    let mut child = command
        .spawn()
        .map_err(|error| format!("spawn ffmpeg poster: {error}"))?;
    let Some(stderr) = child.stderr.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("spawn ffmpeg poster: stderr pipe unavailable".to_string());
    };
    let stderr_reader = match std::thread::Builder::new()
        .name("clipline-poster-stderr".into())
        .spawn(move || read_bounded(stderr, MAX_POSTER_STDERR_BYTES))
    {
        Ok(reader) => reader,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("spawn ffmpeg poster stderr reader: {error}"));
        }
    };

    let wait = wait_for_poster_child(&mut child, timeout);
    let stderr = stderr_reader
        .join()
        .map_err(|_| "ffmpeg poster stderr reader panicked".to_string())?
        .map_err(|error| format!("read ffmpeg poster stderr: {error}"))?;
    match wait.map_err(|error| format!("wait for ffmpeg poster: {error}"))? {
        PosterChildWait::Exited(status) => Ok((status, stderr)),
        PosterChildWait::TimedOut => Err(format!(
            "ffmpeg poster timed out after {} seconds",
            timeout.as_secs()
        )),
    }
}

fn read_bounded(mut reader: impl Read, max_bytes: usize) -> io::Result<Vec<u8>> {
    let mut retained = Vec::new();
    let mut chunk = [0u8; 8 * 1024];
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            return Ok(retained);
        }
        let keep = read.min(max_bytes.saturating_sub(retained.len()));
        retained.extend_from_slice(&chunk[..keep]);
    }
}

struct PosterTemp {
    path: PathBuf,
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
                Ok(_) => return Ok(Self { path, armed: true }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(format!("reserve poster temp: {error}")),
            }
        }
        Err("reserve poster temp: unique-name attempts exhausted".to_string())
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn publish(mut self, poster: &Path) -> Result<(), String> {
        atomic_replace_file(&self.path, poster)
            .map_err(|error| format!("finalize poster: {error}"))?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for PosterTemp {
    fn drop(&mut self) {
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
                None
            }),
            None
        );
        assert!(
            cache.get().is_none(),
            "a missing executable must not initialize the cache"
        );

        let expected = PathBuf::from(r"C:\tools\ffmpeg.exe");
        assert_eq!(
            cached_successful_path(&cache, || {
                attempts.set(attempts.get() + 1);
                Some(expected.clone())
            }),
            Some(expected.clone())
        );
        assert_eq!(
            cached_successful_path(&cache, || {
                attempts.set(attempts.get() + 1);
                Some(PathBuf::from(r"C:\other\ffmpeg.exe"))
            }),
            Some(expected)
        );
        assert_eq!(attempts.get(), 2, "a success must remain cached");
    }

    #[test]
    fn seek_arg_clamps_negative_and_keeps_millisecond_precision() {
        assert_eq!(seek_arg(12.5), "12.500");
        assert_eq!(seek_arg(0.0), "0.000");
        assert_eq!(seek_arg(-3.0), "0.000");
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
        let temp = PosterTemp::reserve(&poster).unwrap();
        let temp_path = temp.path().to_path_buf();
        std::fs::write(&temp_path, b"complete jpeg").unwrap();

        temp.publish(&poster).unwrap();

        assert_eq!(std::fs::read(&poster).unwrap(), b"complete jpeg");
        assert!(!temp_path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn timed_out_poster_child_is_killed_and_reaped() {
        let mut child = Command::new(std::env::current_exe().expect("locate test executable"))
            .args([
                "--exact",
                "poster::tests::poster_timeout_fixture_sleeps",
                "--nocapture",
            ])
            .env("CLIPLINE_POSTER_TIMEOUT_FIXTURE", "1")
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
    fn poster_timeout_fixture_sleeps() {
        if std::env::var("CLIPLINE_POSTER_TIMEOUT_FIXTURE").as_deref() == Ok("1") {
            std::thread::sleep(std::time::Duration::from_secs(30));
        }
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
