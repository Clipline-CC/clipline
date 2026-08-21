//! Library poster frames: a cached JPEG thumbnail beside each clip
//! (`<clip>.poster.jpg`), extracted with ffmpeg at a representative moment
//! (chosen by the caller — typically the first event marker). The gallery
//! loads these through the asset protocol, the same path clips play back
//! through, so no new scope is needed.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::library::suppress_console;

/// Poster width in pixels. Cards render ~250px wide; 480 covers 2x displays
/// while keeping each JPEG to a few tens of KB. Height follows the aspect
/// ratio (`-2` keeps it even for the encoder).
const POSTER_WIDTH: u32 = 480;
const POSTER_EXTRACTION_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_POSTER_STDOUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_POSTER_STDERR_BYTES: usize = 64 * 1024;
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
    let mut cmd = poster_command(ffmpeg, clip, seek_s);
    let output = run_poster_child(&mut cmd, POSTER_EXTRACTION_TIMEOUT)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr.bytes);
        return Err(format!("ffmpeg poster failed: {stderr}"));
    }
    publish_poster_output(poster, output.stdout)
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
    let mut tmp = crate::image::SiblingTemp::reserve(poster)?;
    tmp.write_all(&stdout.bytes)?;
    tmp.publish(poster)
}

fn poster_command(ffmpeg: &Path, clip: &Path, seek_s: f64) -> Command {
    let mut cmd = Command::new(ffmpeg);
    suppress_console(&mut cmd);
    // Input-side seek is a fast keyframe seek — fine for a thumbnail.
    // PNG screenshots are single-frame: seeking past frame 1 yields no output.
    let is_png = clip
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("png"));
    cmd.args(["-hide_banner", "-nostdin"]);
    if !is_png {
        cmd.args(["-ss", &seek_arg(seek_s)]);
    }
    cmd.arg("-i")
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

fn run_poster_child(command: &mut Command, timeout: Duration) -> Result<PosterChildOutput, String> {
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
) -> Result<PosterChildOutput, String> {
    let mut child = command
        .spawn()
        .map_err(|error| format!("spawn ffmpeg poster: {error}"))?;
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("spawn ffmpeg poster: stdout or stderr pipe unavailable".to_string());
    };
    let stdout_reader =
        match spawn_bounded_reader("clipline-poster-stdout", stdout, max_stdout_bytes) {
            Ok(reader) => reader,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("spawn ffmpeg poster stdout reader: {error}"));
            }
        };
    let stderr_reader =
        match spawn_bounded_reader("clipline-poster-stderr", stderr, max_stderr_bytes) {
            Ok(reader) => reader,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                return Err(format!("spawn ffmpeg poster stderr reader: {error}"));
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
        )),
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
    use std::io::Write;
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
        assert!(
            args.windows(2).any(|pair| pair[0] == "-i"
                && pair[1] == clip.to_string_lossy()),
            "the clip path must be an input, never an output file"
        );
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
    fn png_poster_command_skips_the_seek_so_a_single_frame_survives() {
        let clip = Path::new(r"C:\clips\shot.png");
        let command = poster_command(Path::new(r"C:\tools\ffmpeg.exe"), clip, 2.5);
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(
            !args.iter().any(|arg| arg == "-ss"),
            "a still image has one frame; seeking past it yields no output"
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
        let first = crate::image::SiblingTemp::reserve(&poster).unwrap();
        let second = crate::image::SiblingTemp::reserve(&poster).unwrap();
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
        let mut temp = crate::image::SiblingTemp::reserve(&poster).unwrap();
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
