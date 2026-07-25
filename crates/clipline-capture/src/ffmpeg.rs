//! FFmpeg subprocess discovery and encoder probing (ddoc §4).
//!
//! Clipline drives a bundled `ffmpeg.exe` (LGPL shared build) as a child
//! process rather than linking libavcodec — see the milestone plan for the
//! rationale (no unsafe FFI, version-robust, clean LGPL separation). This
//! module locates the binary and reports which of our target encoders it
//! can actually use on this machine. `ffmpeg -encoders` lists every
//! *compiled* encoder regardless of hardware, so each hardware encoder is
//! confirmed with a one-frame test encode before being reported.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::probe::{Codec, EncoderApi, EncoderBackend, EncoderCapability};

/// Stop Windows from flashing a console window for each `ffmpeg` child we
/// spawn (startup probing alone launches ~11 of them). No-op off Windows.
#[cfg(windows)]
pub fn suppress_console(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
pub fn suppress_console(_cmd: &mut Command) {}

/// The FFmpeg encoder names Clipline targets, mapped to (backend, codec).
/// Software AV1 is SVT-AV1 (LGPL-clean); no GPL x264/x265, no software HEVC.
const KNOWN_ENCODERS: &[(&str, EncoderBackend, Codec)] = &[
    ("h264_nvenc", EncoderBackend::Nvenc, Codec::H264),
    ("hevc_nvenc", EncoderBackend::Nvenc, Codec::Hevc),
    ("av1_nvenc", EncoderBackend::Nvenc, Codec::Av1),
    ("h264_amf", EncoderBackend::Amf, Codec::H264),
    ("hevc_amf", EncoderBackend::Amf, Codec::Hevc),
    ("av1_amf", EncoderBackend::Amf, Codec::Av1),
    ("h264_qsv", EncoderBackend::QuickSync, Codec::H264),
    ("hevc_qsv", EncoderBackend::QuickSync, Codec::Hevc),
    ("av1_qsv", EncoderBackend::QuickSync, Codec::Av1),
    ("h264_mf", EncoderBackend::MfSoftware, Codec::H264),
    ("libsvtav1", EncoderBackend::SvtAv1, Codec::Av1),
];

/// The FFmpeg `-c:v` encoder name for a (backend, codec) pair, if Clipline
/// targets it. Used by `FfmpegVideoEncoder` to build the child command.
pub fn encoder_name(backend: EncoderBackend, codec: Codec) -> Option<&'static str> {
    KNOWN_ENCODERS
        .iter()
        .find(|(_, b, c)| *b == backend && *c == codec)
        .map(|(name, _, _)| *name)
}

/// FFmpeg's encoder list only describes compiled support. Hardware encoders
/// and Media Foundation's optional system encoder must prove they can encode
/// on this machine. SVT-AV1 is the only bundled, self-contained exception.
fn requires_test_encode(backend: EncoderBackend) -> bool {
    backend != EncoderBackend::SvtAv1
}

/// Parse `ffmpeg -encoders` output into the subset of [`KNOWN_ENCODERS`]
/// that FFmpeg was compiled with. Pure string work — the encoder name is
/// the second column on lines whose first column is the 6-char flag field
/// (e.g. `V....D`), which avoids matching names that appear in a
/// description.
pub fn parse_available_encoders(encoders_output: &str) -> Vec<(EncoderBackend, Codec)> {
    let mut found = Vec::new();
    for line in encoders_output.lines() {
        let mut tokens = line.split_whitespace();
        let Some(flags) = tokens.next() else { continue };
        if flags.len() != 6 || !flags.chars().all(|c| c == '.' || c.is_ascii_uppercase()) {
            continue;
        }
        let Some(name) = tokens.next() else { continue };
        if let Some((_, backend, codec)) = KNOWN_ENCODERS.iter().find(|(n, _, _)| *n == name) {
            found.push((*backend, *codec));
        }
    }
    found
}

/// Group flat (backend, codec) pairs into one [`EncoderCapability`] per
/// backend (api = FFmpeg), codecs in preference order.
fn group_capabilities(mut pairs: Vec<(EncoderBackend, Codec)>) -> Vec<EncoderCapability> {
    pairs.sort();
    pairs.dedup();
    let mut caps: Vec<EncoderCapability> = Vec::new();
    for (backend, codec) in pairs {
        match caps.iter_mut().find(|c| c.backend == backend) {
            Some(cap) => cap.codecs.push(codec),
            None => caps.push(EncoderCapability {
                api: EncoderApi::Ffmpeg,
                backend,
                codecs: vec![codec],
            }),
        }
    }
    for cap in &mut caps {
        cap.codecs.sort();
    }
    caps
}

static BUNDLED_FFMPEG: OnceLock<PathBuf> = OnceLock::new();

/// Register the packaged ffmpeg resource path discovered by the desktop shell.
/// The explicit environment override remains first so developers and users can
/// intentionally replace the subprocess binary.
pub fn set_bundled_ffmpeg(path: PathBuf) {
    let _ = BUNDLED_FFMPEG.set(path);
}

fn ffmpeg_exe_name() -> &'static str {
    if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    }
}

fn search_paths_from(
    exe_name: &str,
    explicit: Option<PathBuf>,
    current_exe: Option<PathBuf>,
    local_appdata: Option<PathBuf>,
    appdata: Option<PathBuf>,
    bundled: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut push_unique = |path: PathBuf| {
        if !paths.contains(&path) {
            paths.push(path);
        }
    };
    if let Some(explicit) = explicit {
        push_unique(explicit);
    }
    if let Some(bundled) = bundled {
        push_unique(bundled);
    }
    if let Some(exe) = current_exe {
        if let Some(dir) = exe.parent() {
            push_unique(dir.join(exe_name));
        }
    }
    if let Some(local_appdata) = local_appdata {
        push_unique(local_appdata.join("Clipline").join("ffmpeg").join(exe_name));
    }
    if let Some(appdata) = appdata {
        push_unique(appdata.join("Clipline").join("ffmpeg").join(exe_name));
    }
    push_unique(PathBuf::from(exe_name)); // PATH fallback
    paths
}

/// Candidate locations for `ffmpeg`, most-specific first: an explicit
/// `CLIPLINE_FFMPEG` override, the packaged app resource, next to our own exe,
/// an installed runtime under `%LOCALAPPDATA%\Clipline\ffmpeg`, the per-user
/// `%APPDATA%\Clipline\ffmpeg` override, then a bare name for a PATH lookup.
pub fn search_paths() -> Vec<PathBuf> {
    search_paths_from(
        ffmpeg_exe_name(),
        std::env::var_os("CLIPLINE_FFMPEG").map(PathBuf::from),
        std::env::current_exe().ok(),
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from),
        std::env::var_os("APPDATA").map(PathBuf::from),
        BUNDLED_FFMPEG.get().cloned(),
    )
}

/// Locate a runnable `ffmpeg`: the first search path that answers
/// `-version` with success. `None` means the FFmpeg encoder tier is simply
/// unavailable (CI, or a machine without the bundle) — never an error.
pub fn locate() -> Option<PathBuf> {
    search_paths().into_iter().find(|path| runs(path))
}

/// All probe subprocesses must finish well within this; a wedged ffmpeg is
/// killed rather than allowed to block startup probing indefinitely.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_PROBE_STDOUT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug)]
pub(crate) enum ChildWait {
    Exited(ExitStatus),
    TimedOut,
}

/// Wait for a child without ever leaving it running past `timeout`. Timeout
/// and `try_wait` failure paths both attempt to kill and reap before returning.
pub(crate) fn wait_for_child(child: &mut Child, timeout: Duration) -> io::Result<ChildWait> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(ChildWait::Exited(status)),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(ChildWait::TimedOut);
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        }
    }
}

fn read_bounded(mut reader: impl Read, max_bytes: usize) -> io::Result<Vec<u8>> {
    let mut retained = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            return Ok(retained);
        }
        let keep = read.min(max_bytes.saturating_sub(retained.len()));
        retained.extend_from_slice(&chunk[..keep]);
    }
}

/// Run a probe command to completion, killing it if it exceeds
/// `PROBE_TIMEOUT`. stdout is captured (these commands emit little); stderr is
/// discarded. `None` on spawn failure or timeout — treated as "unavailable".
fn run_bounded(mut cmd: Command) -> Option<Output> {
    run_bounded_with_timeout(&mut cmd, PROBE_TIMEOUT, MAX_PROBE_STDOUT_BYTES)
}

fn run_bounded_with_timeout(
    cmd: &mut Command,
    timeout: Duration,
    max_stdout_bytes: usize,
) -> Option<Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    suppress_console(cmd);
    let mut child = cmd.spawn().ok()?;
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let reader = match std::thread::Builder::new()
        .name("clipline-ffmpeg-probe-reader".into())
        .spawn(move || read_bounded(stdout, max_stdout_bytes))
    {
        Ok(reader) => reader,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };
    let wait = wait_for_child(&mut child, timeout);
    let stdout = reader.join().ok()?.ok()?;
    let ChildWait::Exited(status) = wait.ok()? else {
        return None;
    };
    Some(Output {
        status,
        stdout,
        stderr: Vec::new(),
    })
}

fn runs(path: &Path) -> bool {
    let mut cmd = Command::new(path);
    cmd.args(["-hide_banner", "-version"]);
    run_bounded(cmd)
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Confirm a hardware encoder actually works on this machine with a
/// one-frame test encode discarded to the null muxer. The probe size is
/// 640x360, not a tiny placeholder: AMF rejects very small resolutions
/// (`Init() failed with error 5` at 128x72), which would wrongly drop a
/// working H.264/HEVC encoder.
fn test_encode(ffmpeg: &Path, encoder: &str, backend: EncoderBackend) -> bool {
    let mut cmd = Command::new(ffmpeg);
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=640x360:rate=30",
        "-frames:v",
        "1",
        "-c:v",
        encoder,
    ]);
    if backend == EncoderBackend::MfSoftware {
        cmd.args(["-hw_encoding", "0"]);
    }
    cmd.args(["-f", "null", "-"]);
    run_bounded(cmd)
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Probe one located `ffmpeg` binary: list compiled encoders, then confirm
/// each hardware encoder with a test encode (software is trusted).
fn probe_ffmpeg(ffmpeg: &Path) -> Vec<EncoderCapability> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-encoders"]);
    let Some(output) = run_bounded(cmd) else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let listed = parse_available_encoders(&String::from_utf8_lossy(&output.stdout));
    let usable: Vec<(EncoderBackend, Codec)> = listed
        .into_iter()
        .filter(|&(backend, codec)| {
            !requires_test_encode(backend)
                || encoder_name(backend, codec)
                    .map(|name| test_encode(ffmpeg, name, backend))
                    .unwrap_or(false)
        })
        .collect();
    group_capabilities(usable)
}

/// Locate FFmpeg and probe it. Empty when no usable FFmpeg is present —
/// the app falls back to the MFT-only matrix exactly as before.
pub fn probe() -> Vec<EncoderCapability> {
    match locate() {
        Some(ffmpeg) => probe_ffmpeg(&ffmpeg),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const PROBE_CHILD_MODE: &str = "CLIPLINE_FFMPEG_PROBE_CHILD_MODE";

    #[test]
    fn probe_subprocess_helper() {
        match std::env::var(PROBE_CHILD_MODE).as_deref() {
            Ok("burst") => {
                std::io::stdout()
                    .write_all(&vec![b'X'; 8 * 1024 * 1024])
                    .expect("write burst");
                std::io::stdout().flush().expect("flush burst");
            }
            Ok("hang") => std::thread::sleep(Duration::from_secs(60)),
            _ => {}
        }
    }

    fn probe_helper_command(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().expect("current test executable"));
        command
            .args([
                "--exact",
                "ffmpeg::tests::probe_subprocess_helper",
                "--nocapture",
            ])
            .env(PROBE_CHILD_MODE, mode);
        command
    }

    // A trimmed real `ffmpeg -encoders` excerpt (FFmpeg 8.x): the AMD box
    // lists nvenc/qsv/amf and libsvtav1/libx265 even without that hardware.
    const ENCODERS: &str = "\
 Encoders:
 V..... = Video
 ------
 V....D libaom-av1           libaom AV1 (codec av1)
 V..... libsvtav1            SVT-AV1 encoder (codec av1)
 V....D av1_nvenc            NVIDIA NVENC av1 encoder (codec av1)
 V..... av1_qsv              AV1 (Intel Quick Sync Video) (codec av1)
 V....D av1_amf              AMD AMF AV1 encoder (codec av1)
 V....D h264_amf             AMD AMF H.264 Encoder (codec h264)
 V....D h264_nvenc           NVIDIA NVENC H.264 encoder (codec h264)
 V..... h264_mf              MediaFoundation H.264 encoder (codec h264)
 V....D libx265              libx265 H.265 / HEVC (codec hevc)
 V....D hevc_amf             AMD AMF HEVC encoder (codec hevc)
 V....D hevc_nvenc           NVIDIA NVENC hevc encoder (codec hevc)
 V..... hevc_qsv             HEVC (Intel Quick Sync Video) (codec hevc)
 V..... h264_qsv             H.264 (Intel Quick Sync Video) (codec h264)
 A....D aac                  AAC (Advanced Audio Coding)";

    #[test]
    fn verbose_probe_is_drained_concurrently_and_retained_output_is_bounded() {
        let mut command = probe_helper_command("burst");
        let output = run_bounded_with_timeout(&mut command, Duration::from_secs(5), 1024 * 1024)
            .expect("verbose child must not block on a full pipe");

        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 1024 * 1024);
        assert!(
            output.stdout.iter().filter(|byte| **byte == b'X').count() > 900_000,
            "the retained prefix should contain the child's burst"
        );
    }

    #[test]
    fn wedged_probe_is_killed_and_reaped_at_deadline() {
        let mut command = probe_helper_command("hang");
        let started = Instant::now();

        assert!(
            run_bounded_with_timeout(&mut command, Duration::from_millis(100), 1024,).is_none()
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn bounded_reader_keeps_draining_after_reaching_its_limit() {
        let input = vec![0xAB; 4096];
        let retained = read_bounded(std::io::Cursor::new(input), 1000).unwrap();
        assert_eq!(retained, vec![0xAB; 1000]);
    }

    #[test]
    fn parses_known_encoders_and_ignores_others() {
        let found = parse_available_encoders(ENCODERS);
        // libaom-av1 and libx265 are real lines but not Clipline targets.
        assert!(!found.contains(&(EncoderBackend::SvtAv1, Codec::Hevc)));
        assert!(found.contains(&(EncoderBackend::SvtAv1, Codec::Av1)));
        assert!(found.contains(&(EncoderBackend::Nvenc, Codec::H264)));
        assert!(found.contains(&(EncoderBackend::Amf, Codec::Hevc)));
        assert!(found.contains(&(EncoderBackend::QuickSync, Codec::Av1)));
        assert!(found.contains(&(EncoderBackend::MfSoftware, Codec::H264)));
        assert_eq!(found.len(), 11, "9 hw + 2 software encoders");
    }

    #[test]
    fn name_appearing_only_in_a_description_is_not_matched() {
        // The encoder-name column is column 2; a name in prose must not match.
        let prose = " V....D somecodec            uses h264_nvenc internally (codec foo)";
        assert!(parse_available_encoders(prose).is_empty());
    }

    #[test]
    fn grouping_yields_one_capability_per_backend_with_sorted_codecs() {
        let caps = group_capabilities(vec![
            (EncoderBackend::Amf, Codec::H264),
            (EncoderBackend::Amf, Codec::Av1),
            (EncoderBackend::Nvenc, Codec::H264),
            (EncoderBackend::Amf, Codec::H264), // duplicate
        ]);
        assert_eq!(caps.len(), 2);
        let amf = caps
            .iter()
            .find(|c| c.backend == EncoderBackend::Amf)
            .unwrap();
        assert_eq!(amf.api, EncoderApi::Ffmpeg);
        assert_eq!(
            amf.codecs,
            vec![Codec::H264, Codec::Av1],
            "preference order, deduped"
        );
    }

    #[test]
    fn encoder_name_round_trips_known_pairs() {
        assert_eq!(
            encoder_name(EncoderBackend::Amf, Codec::Hevc),
            Some("hevc_amf")
        );
        assert_eq!(
            encoder_name(EncoderBackend::SvtAv1, Codec::Av1),
            Some("libsvtav1")
        );
        // Media Foundation supplies LGPL-compatible software H.264 on Windows.
        assert_eq!(encoder_name(EncoderBackend::SvtAv1, Codec::H264), None);
        assert_eq!(
            encoder_name(EncoderBackend::MfSoftware, Codec::H264),
            Some("h264_mf")
        );
    }

    #[test]
    fn software_media_foundation_requires_a_test_encode() {
        assert!(requires_test_encode(EncoderBackend::MfSoftware));
        assert!(requires_test_encode(EncoderBackend::Nvenc));
        assert!(!requires_test_encode(EncoderBackend::SvtAv1));
    }

    #[test]
    fn search_paths_end_with_a_bare_path_lookup() {
        let paths = search_paths();
        let last = paths.last().unwrap();
        assert!(last.as_os_str() == "ffmpeg" || last.as_os_str() == "ffmpeg.exe");
    }

    fn fixture_path(parts: &[&str]) -> PathBuf {
        parts.iter().collect()
    }

    #[test]
    fn bundled_ffmpeg_resource_wins_over_appdata_and_path() {
        let install_exe = fixture_path(&["clipline-install", "Clipline.exe"]);
        let appdata = fixture_path(&["user-profile", "AppData", "Roaming"]);
        let bundled = fixture_path(&["clipline-install", "resources", "ffmpeg", "ffmpeg.exe"]);

        let paths = search_paths_from(
            "ffmpeg.exe",
            None,
            Some(install_exe),
            None,
            Some(appdata.clone()),
            Some(bundled.clone()),
        );

        assert_eq!(paths[0], bundled);
        assert_eq!(paths[1], fixture_path(&["clipline-install", "ffmpeg.exe"]));
        assert_eq!(
            paths[2],
            appdata.join("Clipline").join("ffmpeg").join("ffmpeg.exe")
        );
    }

    #[test]
    fn explicit_ffmpeg_override_stays_first() {
        let explicit = fixture_path(&["tools", "ffmpeg.exe"]);
        let bundled = fixture_path(&["clipline-install", "resources", "ffmpeg", "ffmpeg.exe"]);

        let paths = search_paths_from(
            "ffmpeg.exe",
            Some(explicit.clone()),
            Some(fixture_path(&["clipline-install", "Clipline.exe"])),
            None,
            None,
            Some(bundled.clone()),
        );

        assert_eq!(paths[0], explicit);
        assert_eq!(paths[1], bundled);
    }

    #[test]
    fn installed_local_appdata_runtime_precedes_roaming_override_and_path() {
        let local_appdata = fixture_path(&["user-profile", "AppData", "Local"]);
        let appdata = fixture_path(&["user-profile", "AppData", "Roaming"]);

        let paths = search_paths_from(
            "ffmpeg.exe",
            None,
            None,
            Some(local_appdata.clone()),
            Some(appdata.clone()),
            None,
        );

        assert_eq!(
            paths,
            [
                local_appdata
                    .join("Clipline")
                    .join("ffmpeg")
                    .join("ffmpeg.exe"),
                appdata.join("Clipline").join("ffmpeg").join("ffmpeg.exe"),
                PathBuf::from("ffmpeg.exe"),
            ]
        );
    }

    #[test]
    fn duplicate_candidates_keep_the_first_occurrence() {
        let install_dir = fixture_path(&["clipline-install"]);
        let installed_ffmpeg = install_dir.join("ffmpeg.exe");
        let appdata = fixture_path(&["user-profile", "AppData"]);
        let appdata_ffmpeg = appdata.join("Clipline").join("ffmpeg").join("ffmpeg.exe");

        let paths = search_paths_from(
            "ffmpeg.exe",
            Some(installed_ffmpeg.clone()),
            Some(install_dir.join("Clipline.exe")),
            Some(appdata.clone()),
            Some(appdata),
            Some(installed_ffmpeg.clone()),
        );

        assert_eq!(
            paths,
            [
                installed_ffmpeg,
                appdata_ffmpeg,
                PathBuf::from("ffmpeg.exe"),
            ]
        );
    }
}
