//! Native PrintWindow experiment; never imported by a production backend.
use super::protocol::{MAGIC, Packet, frame_bytes, mock_counter, read_packet, validate_times};
use clipline_capture::ffmpeg_encoder::FfmpegVideoEncoder;
use clipline_capture::probe::{Codec, EncoderBackend};
use clipline_capture::windows::{
    MftConfig, MftH264Encoder, WasapiLoopback, d3d11, qpc_now_ticks_100ns,
};
use clipline_capture::{
    CaptureEngine, CaptureError, Encoder, Frame, FrameData, Recorder, RelativeClock,
};
use std::error::Error;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
use windows::Win32::System::JobObjects::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::WindowsAndMessaging::*;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

struct Worker {
    child: Child,
    input: Option<ChildStdin>,
    receiver: Option<Receiver<std::io::Result<Packet>>>,
    reader: Option<JoinHandle<()>>,
    _job: Job,
}
struct Job(HANDLE);
impl Job {
    fn new() -> Result<Self> {
        // SAFETY: unnamed job with non-inheritable default security. Closing the
        // parent's sole handle kills helpers even if the parent is terminated.
        unsafe {
            let job = Self(CreateJobObjectW(None, None)?);
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                std::mem::size_of_val(&limits) as u32,
            )?;
            Ok(job)
        }
    }
}
impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: owns this job handle; child never inherits it.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
impl Worker {
    fn new(hwnd: isize, pid: u32, stderr: File) -> Result<Self> {
        Self::spawn(
            Command::new(std::env::current_exe()?)
                .args(["--worker", &hwnd.to_string(), &pid.to_string()])
                .stderr(stderr),
        )
    }
    fn spawn(command: &mut Command) -> Result<Self> {
        let job = Job::new()?;
        let mut child = command
            .creation_flags(0x08000000)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        // SAFETY: handles belong to the live job/child. Assign before any request;
        // a failed assignment must never leave an unsupervised helper running.
        if let Err(error) =
            unsafe { AssignProcessToJobObject(job.0, HANDLE(child.as_raw_handle())) }
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error.into());
        }
        let input = child.stdin.take();
        let mut output = child.stdout.take().ok_or("child stdout missing")?;
        let (sender, receiver) = mpsc::sync_channel(1);
        let reader = thread::spawn(move || {
            let mut previous = 0;
            for sequence in 0.. {
                let packet = read_packet(&mut output, sequence, previous);
                let failed = packet.is_err();
                if let Ok(packet) = &packet {
                    previous = packet.begin;
                }
                if sender.send(packet).is_err() || failed {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            input,
            receiver: Some(receiver),
            reader: Some(reader),
            _job: job,
        })
    }
    fn sample(&mut self) -> Result<Packet> {
        let result = (|| -> Result<Packet> {
            self.input
                .as_mut()
                .ok_or("worker closed")?
                .write_all(&[1])?;
            Ok(self
                .receiver
                .as_ref()
                .ok_or("worker closed")?
                .recv_timeout(Duration::from_secs(2))
                .map_err(|e| format!("capture worker failed/deadline: {e}"))??)
        })();
        if result.is_err() {
            self.shutdown();
        }
        result
    }
    fn shutdown(&mut self) {
        self.input.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.receiver.take(); // unblock a reader sending EOF behind a buffered packet
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct Target {
    hwnd: HWND,
    pid: u32,
    process: HANDLE,
}
impl Target {
    fn new(hwnd: isize, pid: u32) -> Result<Self> {
        // SAFETY: query-only owned process handle, released by Drop. A held handle
        // plus aliveness checks prevents PID reuse from substituting another process.
        let process = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                pid,
            )?
        };
        let target = Self {
            hwnd: HWND(hwnd as *mut _),
            pid,
            process,
        };
        target.geometry()?;
        Ok(target)
    }
    fn geometry(&self) -> Result<(RECT, RECT, POINT)> {
        let (mut owner, mut affinity) = (0, 0);
        let (mut window, mut client, mut origin) =
            (RECT::default(), RECT::default(), POINT::default());
        // SAFETY: borrowed HWND and initialized out-parameters; no target mutation.
        unsafe {
            GetWindowThreadProcessId(self.hwnd, Some(&mut owner));
            if owner != self.pid || WaitForSingleObject(self.process, 0) != WAIT_TIMEOUT {
                return Err("target identity ended or changed".into());
            }
            GetWindowDisplayAffinity(self.hwnd, &mut affinity)?;
            if affinity != 0
                || !IsWindowVisible(self.hwnd).as_bool()
                || IsIconic(self.hwnd).as_bool()
            {
                return Err("target protected, hidden or minimized".into());
            }
            GetWindowRect(self.hwnd, &mut window)?;
            GetClientRect(self.hwnd, &mut client)?;
            if !ClientToScreen(self.hwnd, &mut origin).as_bool() {
                return Err("client origin unavailable".into());
            }
        }
        frame_bytes(
            (window.right - window.left) as u32,
            (window.bottom - window.top) as u32,
        )?;
        frame_bytes(client.right as u32, client.bottom as u32)?;
        let (x, y) = (origin.x - window.left, origin.y - window.top);
        if x < 0
            || y < 0
            || x + client.right > window.right - window.left
            || y + client.bottom > window.bottom - window.top
        {
            return Err("client outside captured window".into());
        }
        Ok((window, client, origin))
    }
}
impl Drop for Target {
    fn drop(&mut self) {
        // SAFETY: this owns a process handle, never a borrowed DWM surface handle.
        unsafe {
            let _ = CloseHandle(self.process);
        }
    }
}

struct Dib {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut std::ffi::c_void,
    size: usize,
}
impl Dib {
    fn new(width: u32, height: u32) -> Result<Self> {
        let size = frame_bytes(width, height)?;
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        // SAFETY: owned memory DC and top-down DIB. No screen readback occurs.
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                return Err("CreateCompatibleDC failed".into());
            }
            let mut dib = Self {
                dc,
                bitmap: HBITMAP::default(),
                old: HGDIOBJ::default(),
                bits: std::ptr::null_mut(),
                size,
            };
            dib.bitmap = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut dib.bits, None, 0)?;
            dib.old = SelectObject(dc, dib.bitmap.into());
            if dib.old.is_invalid() || dib.bits.is_null() {
                return Err("SelectObject or DIB allocation failed".into());
            }
            Ok(dib)
        }
    }
}
impl Drop for Dib {
    fn drop(&mut self) {
        // SAFETY: restore original selection before deleting owned DIB/DC.
        unsafe {
            if !self.old.is_invalid() {
                SelectObject(self.dc, self.old);
            }
            if !self.bitmap.is_invalid() {
                let _ = DeleteObject(self.bitmap.into());
            }
            let _ = DeleteDC(self.dc);
        }
    }
}

fn worker(hwnd: isize, pid: u32) -> Result<()> {
    // SAFETY: called before any window/geometry work in this isolated process.
    unsafe {
        let _ = SetProcessDPIAware();
    }
    let target = Target::new(hwnd, pid)?;
    let initial = target.geometry()?;
    let width = (initial.0.right - initial.0.left) as u32;
    let height = (initial.0.bottom - initial.0.top) as u32;
    let dib = Dib::new(width, height)?;
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    let mut request = [0];
    for sequence in 0u64.. {
        if input.read(&mut request)? == 0 {
            break;
        }
        if request != [1] {
            return Err("invalid capture request".into());
        }
        let before = target.geometry()?;
        if before.0.right - before.0.left != width as i32
            || before.0.bottom - before.0.top != height as i32
            || before.1 != initial.1
        {
            return Err("target resized; restart the fixed-resolution experiment".into());
        }
        let begin = qpc_now_ticks_100ns()?;
        // SAFETY: valid selected DIB and validated HWND. Initialize to expose
        // incomplete painting; GdiFlush synchronizes access to the DIB's bits.
        unsafe {
            std::ptr::write_bytes(dib.bits, 0xcd, dib.size);
            if !PrintWindow(target.hwnd, dib.dc, PRINT_WINDOW_FLAGS(2)).as_bool() {
                return Err("PrintWindow returned false".into());
            }
            if !GdiFlush().as_bool() {
                return Err("GdiFlush failed".into());
            }
        }
        let end = qpc_now_ticks_100ns()?;
        if target.geometry()? != before {
            return Err("target geometry changed during capture".into());
        }
        let (cw, ch) = (before.1.right as u32, before.1.bottom as u32);
        let length = frame_bytes(cw, ch)?;
        for value in [MAGIC, cw, ch, length as u32] {
            output.write_all(&value.to_le_bytes())?;
        }
        output.write_all(&sequence.to_le_bytes())?;
        output.write_all(&begin.to_le_bytes())?;
        output.write_all(&end.to_le_bytes())?;
        let x = (before.2.x - before.0.left) as usize;
        let y = (before.2.y - before.0.top) as usize;
        // SAFETY: validated crop bounds, live allocation, GDI has finished writing.
        let pixels = unsafe { std::slice::from_raw_parts(dib.bits.cast::<u8>(), dib.size) };
        for row in y..y + ch as usize {
            let start = (row * width as usize + x) * 4;
            output.write_all(&pixels[start..start + cw as usize * 4])?;
        }
        output.flush()?;
    }
    Ok(())
}

struct Capture {
    worker: Worker,
    device: ID3D11Device,
    clock: RelativeClock,
    dimensions: (u32, u32),
    previous: i64,
    deadline: Instant,
    next: Instant,
    interval: Duration,
    log: File,
    mock: bool,
    counter: Option<u16>,
    progress: Instant,
    count: u64,
}
impl Capture {
    fn sample(&mut self) -> Result<Option<Frame>> {
        if Instant::now() >= self.deadline {
            return Ok(None);
        }
        thread::sleep(self.next.saturating_duration_since(Instant::now()));
        let packet = self.worker.sample()?;
        validate_times(packet.begin, packet.end, self.previous)?;
        if (packet.width, packet.height) != self.dimensions {
            return Err("target resized".into());
        }
        self.previous = packet.begin;
        let counter = if self.mock {
            Some(mock_counter(&packet.pixels, packet.width, packet.height)?)
        } else {
            None
        };
        if self.mock {
            if self.progress.elapsed() > Duration::from_secs(2) {
                return Err("mock motion stalled for two seconds".into());
            }
            if let (Some(now), Some(previous)) = (counter, self.counter) {
                let delta = now.wrapping_sub(previous);
                if delta > 4096 {
                    return Err("mock counter moved backward or tore".into());
                }
                if delta > 0 {
                    self.progress = Instant::now();
                }
            }
            self.counter = counter;
        }
        let pts = self.clock.pts_s(packet.begin); // observation start, NOT a present timestamp
        writeln!(
            self.log,
            "{},{pts:.7},{:.4},{}",
            self.count,
            (packet.end - packet.begin) as f64 / 10_000.0,
            counter.map(|c| c.to_string()).unwrap_or_default()
        )?;
        self.log.flush()?;
        self.count += 1;
        self.next += self.interval;
        if self.next < Instant::now() {
            self.next = Instant::now();
        } // no catch-up duplicates
        let desc = D3D11_TEXTURE2D_DESC {
            Width: packet.width,
            Height: packet.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            ..Default::default()
        };
        let data = D3D11_SUBRESOURCE_DATA {
            pSysMem: packet.pixels.as_ptr().cast(),
            SysMemPitch: packet.width * 4,
            SysMemSlicePitch: 0,
        };
        let mut texture = None;
        // SAFETY: validated contiguous BGRA lives through synchronous upload.
        // A new texture avoids overwriting frames retained by the encoder.
        unsafe {
            self.device
                .CreateTexture2D(&desc, Some(&data), Some(&mut texture))?;
        }
        Ok(Some(Frame {
            pts_s: pts,
            data: FrameData::Gpu(texture.ok_or("texture missing")?),
        }))
    }
}
impl CaptureEngine for Capture {
    fn next_frame(&mut self) -> std::result::Result<Option<Frame>, CaptureError> {
        self.sample().map_err(|e| CaptureError::Init(e.to_string()))
    }
}

pub fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--worker") {
        if args.len() != 3 {
            return Err("invalid worker arguments".into());
        }
        return worker(args[1].parse()?, args[2].parse()?);
    }
    if args.is_empty() || args.iter().any(|a| a == "--help") {
        println!(
            "Experimental PrintWindow -> AMD H264 MFT + default WASAPI system loopback -> session/replay\n--hwnd HANDLE --pid PID --out NEW_DIRECTORY [--seconds 30] [--fps 30] [--expect-mock] [--ffmpeg-amf]\nFixed client dimensions; stops on resize/minimize/hang. No WGC/display fallback.\n--ffmpeg-amf explicitly tests the separate FFmpeg encoder path; no automatic encoder fallback."
        );
        return Ok(());
    }
    let mut hwnd = None;
    let mut pid = None;
    let mut out = None;
    let mut seconds = 30u64;
    let mut fps = 30u32;
    let mut mock = false;
    let mut ffmpeg_encoder = false;
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        if flag == "--expect-mock" {
            mock = true;
            continue;
        }
        if flag == "--ffmpeg-amf" {
            ffmpeg_encoder = true;
            continue;
        }
        let value = args.next().ok_or("option missing value")?;
        match flag.as_str() {
            "--hwnd" => hwnd = Some(value.parse::<isize>()?),
            "--pid" => pid = Some(value.parse::<u32>()?),
            "--out" => out = Some(PathBuf::from(value)),
            "--seconds" => seconds = value.parse()?,
            "--fps" => fps = value.parse()?,
            _ => return Err(format!("unknown option {flag}").into()),
        }
    }
    if !(3..=600).contains(&seconds) || !(1..=60).contains(&fps) {
        return Err("seconds must be 3..600, fps 1..60".into());
    }
    let hwnd = hwnd.filter(|v| *v > 0).ok_or("positive --hwnd required")?;
    let pid = pid.filter(|v| *v > 0).ok_or("positive --pid required")?;
    let out = out.ok_or("--out required")?;
    fs::create_dir(&out)?; // never overwrite existing evidence
    let outcome = (|| -> Result<()> {
        let mut worker = Worker::new(hwnd, pid, File::create(out.join("worker.stderr.log"))?)?;
        // Geometry discovery is explicitly a preflight read, before the recording clock.
        let probe = worker.sample()?;
        if mock {
            mock_counter(&probe.pixels, probe.width, probe.height)?;
        }
        let (device, _) = d3d11::create_device()?;
        let (w, h) = (probe.width, probe.height);
        let encoder: Box<dyn Encoder> = if ffmpeg_encoder {
            let ffmpeg = clipline_capture::ffmpeg::locate()
                .ok_or("verified LGPL FFmpeg required via CLIPLINE_FFMPEG")?;
            Box::new(FfmpegVideoEncoder::new_on(
                &device,
                &ffmpeg,
                EncoderBackend::Amf,
                Codec::H264,
                w,
                h,
                None,
                w & !1,
                h & !1,
                fps,
                12_000_000,
            )?)
        } else {
            Box::new(MftH264Encoder::new(
                &device,
                w,
                h,
                MftConfig {
                    width: w & !1,
                    height: h & !1,
                    fps,
                    bitrate_bps: 12_000_000,
                    encoder_backend: Some(EncoderBackend::Amf),
                },
            )?)
        };
        let clock = RelativeClock::new(qpc_now_ticks_100ns()?);
        let audio = WasapiLoopback::start(clock)?;
        let started = Instant::now();
        let mut log = File::create(out.join("samples.csv"))?;
        writeln!(log, "sample,observation_start_pts_s,print_ms,mock_counter")?;
        let capture = Capture {
            worker,
            device,
            clock,
            dimensions: (w, h),
            previous: probe.begin,
            deadline: started + Duration::from_secs(seconds),
            next: started,
            interval: Duration::from_secs_f64(1.0 / fps as f64),
            log,
            mock,
            counter: None,
            progress: started,
            count: 0,
        };
        let mut recorder = Recorder::with_retention(capture, encoder, 128 * 1024 * 1024, 12.0)
            .with_audio(Box::new(audio));
        recorder.start_full_session(File::create(out.join("session.mp4"))?)?;
        println!(
            "Recording client {w}x{h}, requested {fps} fps, {seconds}s; default output loopback audio. Experimental PrintWindow only."
        );
        if let Err(error) = recorder.run_to_end() {
            // Preserve valid earlier content, but never label the run successful.
            let tail = recorder.finish_stream();
            let session = recorder.finish_full_session();
            fs::write(
                out.join("partial-finalization.txt"),
                format!("{error}\npartial tail: {tail:?}\npartial session: {session:?}"),
            )?;
            return Err(error.into());
        }
        recorder.finish_full_session()?;
        let (file, end) =
            recorder.save_replay(File::create(out.join("replay.mp4"))?, 10.0, None)?;
        drop(file);
        let summary = format!(
            "completed elapsed_s={:.3} client={w}x{h} requested_fps={fps} replay_end={end:.6} ring_bytes={}\nPrintWindow observation-start timestamps; no present synchronization guarantee.\n",
            started.elapsed().as_secs_f64(),
            recorder.ring_bytes()
        );
        fs::write(out.join("summary.txt"), &summary)?;
        print!("{summary}");
        Ok(())
    })();
    if let Err(error) = &outcome {
        let worker_error = fs::read_to_string(out.join("worker.stderr.log")).unwrap_or_default();
        fs::write(
            out.join("FAILED.txt"),
            format!("{error}\nworker: {worker_error}"),
        )?;
        if !worker_error.is_empty() {
            eprintln!("worker: {}", worker_error.trim());
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sleeper() -> Worker {
        Worker::spawn(
            Command::new("powershell.exe")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "Start-Sleep -Seconds 30",
                ])
                .stderr(Stdio::null()),
        )
        .unwrap()
    }
    #[test]
    fn deadline_reaps_worker_before_returning() {
        let mut worker = sleeper();
        let began = Instant::now();
        assert!(worker.sample().is_err());
        assert!(began.elapsed() < Duration::from_secs(6));
        assert!(worker.child.try_wait().unwrap().is_some());
        assert!(worker.reader.is_none());
    }
    #[test]
    fn shutdown_disconnects_a_blocked_sender() {
        let mut worker = sleeper();
        worker.shutdown();
        let (sender, receiver) = mpsc::sync_channel(1);
        sender.send(Err(std::io::Error::other("first"))).unwrap();
        worker.receiver = Some(receiver);
        worker.reader = Some(thread::spawn(move || {
            let _ = sender.send(Err(std::io::Error::other("second")));
        }));
        let began = Instant::now();
        worker.shutdown();
        assert!(began.elapsed() < Duration::from_secs(2));
    }
    #[test]
    fn closing_job_kills_assigned_child() {
        let job = Job::new().unwrap();
        let mut child = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 30",
            ])
            .creation_flags(0x08000000)
            .spawn()
            .unwrap();
        // SAFETY: live owned handles. Even assignment failure reaps test child.
        let assigned = unsafe { AssignProcessToJobObject(job.0, HANDLE(child.as_raw_handle())) };
        if let Err(error) = assigned {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{error}");
        }
        drop(job);
        // SAFETY: child process handle remains live until Child drops.
        let stopped = unsafe { WaitForSingleObject(HANDLE(child.as_raw_handle()), 5000) };
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(stopped, WAIT_OBJECT_0);
    }
}
