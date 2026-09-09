//! Experimental PrintWindow with a supervised subprocess: no injected code.
use super::qpc_now_ticks_100ns;
use crate::print_protocol::{MAGIC, Packet, frame_bytes, print_layout, read_packet};
use std::error::Error;
use std::io::{Read, Write};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
use windows::Win32::System::JobObjects::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::WindowsAndMessaging::*;
type Result<T> = std::result::Result<T, Box<dyn Error>>;
pub(super) struct Worker {
    pending: Option<Instant>,
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
    pub(super) fn new(hwnd: isize, pid: u32) -> Result<Self> {
        Self::spawn(
            Command::new(std::env::current_exe()?)
                .args([
                    "--clipline-print-worker",
                    &hwnd.to_string(),
                    &pid.to_string(),
                ])
                .stderr(Stdio::null()),
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
            pending: None,
            child,
            input,
            receiver: Some(receiver),
            reader: Some(reader),
            _job: job,
        })
    }
    pub(super) fn sample(&mut self, timeout: Duration) -> Result<Option<Packet>> {
        let result = (|| -> Result<Option<Packet>> {
            if self.pending.is_none() {
                self.input
                    .as_mut()
                    .ok_or("worker closed")?
                    .write_all(&[1])?;
                self.pending = Some(Instant::now());
            }
            let elapsed = self.pending.expect("request pending").elapsed();
            if elapsed >= Duration::from_secs(2) {
                return Err("PrintWindow worker deadline".into());
            }
            match self
                .receiver
                .as_ref()
                .ok_or("worker closed")?
                .recv_timeout(timeout.min(Duration::from_secs(2) - elapsed))
            {
                Ok(packet) => {
                    self.pending = None;
                    Ok(Some(packet?))
                }
                Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
                Err(e) => Err(format!("capture worker failed: {e}").into()),
            }
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

pub(super) struct Target {
    pub(super) hwnd: HWND,
    pub(super) pid: u32,
    process: HANDLE,
}
impl Target {
    pub(super) fn ended(&self) -> bool {
        // SAFETY: held process instance and borrowed HWND, queried without waiting.
        unsafe {
            WaitForSingleObject(self.process, 0) != WAIT_TIMEOUT
                || !IsWindow(Some(self.hwnd)).as_bool()
        }
    }
    pub(super) fn new(hwnd: isize, pid: u32) -> Result<Self> {
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
        target.validate_identity()?;
        Ok(target)
    }
    pub(super) fn validate_identity(&self) -> Result<()> {
        let (mut owner, mut affinity) = (0, 0);
        // SAFETY: read-only queries; the owned process handle pins its instance.
        unsafe {
            GetWindowThreadProcessId(self.hwnd, Some(&mut owner));
            if owner != self.pid || WaitForSingleObject(self.process, 0) != WAIT_TIMEOUT {
                return Err("target identity ended or changed".into());
            }
            GetWindowDisplayAffinity(self.hwnd, &mut affinity)?;
        }
        if affinity != 0 {
            return Err("target has capture protection enabled".into());
        }
        Ok(())
    }
    pub(super) fn geometry(&self) -> Result<(RECT, RECT, POINT)> {
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
    let mut dib: Option<(u32, u32, Dib)> = None;
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
        let window_size = (
            (before.0.right - before.0.left) as u32,
            (before.0.bottom - before.0.top) as u32,
        );
        let (width, height, crop_x, crop_y) = print_layout(
            2,
            window_size,
            (before.1.right as u32, before.1.bottom as u32),
            (
                (before.2.x - before.0.left) as u32,
                (before.2.y - before.0.top) as u32,
            ),
        )?;
        if dib
            .as_ref()
            .is_none_or(|(w, h, _)| (*w, *h) != (width, height))
        {
            dib = Some((width, height, Dib::new(width, height)?));
        }
        let dib = &dib.as_ref().ok_or("DIB missing")?.2;
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
        let x = crop_x as usize;
        let y = crop_y as usize;
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

/// Dispatch before Tauri, singleton locking, logging, or elevation logic.
/// stdout is exclusively the worker protocol.
pub fn run_worker_if_requested() -> bool {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) != Some("--clipline-print-worker") {
        return false;
    }
    let run = || -> Result<()> {
        if args.len() != 3 {
            return Err("invalid PrintWindow worker arguments".into());
        }
        worker(args[1].parse()?, args[2].parse()?)
    };
    if let Err(e) = run() {
        eprintln!("PrintWindow worker: {e}");
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unresponsive_worker_yields_to_recorder_then_is_reaped_at_deadline() {
        let mut worker = Worker::spawn(
            Command::new("powershell.exe")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "[void][Console]::OpenStandardInput().ReadByte(); Start-Sleep -Seconds 30",
                ])
                .stderr(Stdio::null()),
        )
        .unwrap();
        let start = Instant::now();
        assert!(worker.sample(Duration::from_millis(10)).unwrap().is_none());
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "frame poll must remain bounded"
        );
        worker.pending = Some(Instant::now() - Duration::from_secs(3));
        assert!(worker.sample(Duration::ZERO).is_err());
        assert!(worker.child.try_wait().unwrap().is_some());
        assert!(worker.reader.is_none());
    }
}
