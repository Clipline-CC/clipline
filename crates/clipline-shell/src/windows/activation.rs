use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use thiserror::Error;
use windows::core::{HRESULT, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, SetLastError, ERROR_ALREADY_EXISTS, ERROR_NO_DATA,
    ERROR_PIPE_CONNECTED, ERROR_PIPE_LISTENING, ERROR_SUCCESS, FILETIME, GENERIC_READ,
    GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows::Win32::Security::{
    GetLengthSid, GetTokenInformation, IsValidSid, RevertToSelf, TokenUser, TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_FIRST_PIPE_INSTANCE,
    FILE_SHARE_MODE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, ImpersonateNamedPipeClient,
    PeekNamedPipe, WaitNamedPipeW, PIPE_NOWAIT, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE,
};
use windows::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, GetCurrentProcessId, GetCurrentThread, GetProcessTimes,
    OpenProcess, OpenProcessToken, OpenThreadToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

use crate::activation::{
    validate_activation_peer, ActivationCommand, ActivationEnvelope, ActivationPeer,
    MAX_ACTIVATION_PAYLOAD_BYTES,
};
use crate::{ProcessIdentity, ShellCommandSender};

const PIPE_BUFFER_BYTES: u32 = (MAX_ACTIVATION_PAYLOAD_BYTES + 4) as u32;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const IO_TIMEOUT: Duration = Duration::from_millis(500);
const POLL_INTERVAL: Duration = Duration::from_millis(2);
const ACK_ACCEPTED: u8 = 1;
const ACK_REJECTED: u8 = 0;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WindowsActivationError {
    #[error("product identity cannot be empty")]
    EmptyProductIdentity,
    #[error("product identity must contain only ASCII letters, digits, '.', '-', or '_'")]
    InvalidProductIdentity,
    #[error("product identity exceeds 64 bytes")]
    ProductIdentityTooLong,
    #[error("{operation}: {message}")]
    Native {
        operation: &'static str,
        message: String,
    },
    #[error("activation listener startup timed out")]
    ListenerStartupTimeout,
    #[error("activation listener startup failed: {0}")]
    ListenerStartup(String),
    #[error("activation listener thread panicked")]
    ListenerPanicked,
    #[error("activation timed out")]
    Timeout,
    #[error("activation frame ended before the declared payload was complete")]
    IncompleteFrame,
    #[error("activation payload was rejected by the primary instance")]
    Rejected,
    #[error("activation command queue rejected the request: {0}")]
    Queue(String),
    #[error("activation protocol failed: {0}")]
    Protocol(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsInstanceNames {
    pub mutex: String,
    pub pipe: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationAcknowledgement {
    RevealQueued,
    AutostartAcknowledged,
}

pub enum WindowsInstanceRole {
    Primary(WindowsInstanceGuard),
    Secondary(ActivationAcknowledgement),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowsInstanceSnapshot {
    pub listener_alive: bool,
    pub accepted_activations: u64,
    pub rejected_activations: u64,
}

pub struct WindowsInstanceGuard {
    mutex: OwnedHandle,
    pipe_name: String,
    stop: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    accepted: Arc<AtomicU64>,
    rejected: Arc<AtomicU64>,
    join: Option<JoinHandle<()>>,
}

impl WindowsInstanceGuard {
    #[must_use]
    pub fn snapshot(&self) -> WindowsInstanceSnapshot {
        WindowsInstanceSnapshot {
            listener_alive: self.alive.load(Ordering::Acquire),
            accepted_activations: self.accepted.load(Ordering::Relaxed),
            rejected_activations: self.rejected.load(Ordering::Relaxed),
        }
    }

    pub fn shutdown(mut self) -> Result<(), WindowsActivationError> {
        self.shutdown_inner()
    }

    #[doc(hidden)]
    pub fn send_raw_frame_for_test(&self, frame: &[u8]) -> Result<bool, WindowsActivationError> {
        let pipe = connect_client(&self.pipe_name)?;
        write_all(pipe.raw, frame)?;
        read_ack(pipe.raw)
    }

    #[doc(hidden)]
    pub fn send_incomplete_frame_for_test(
        &self,
        frame: &[u8],
    ) -> Result<(), WindowsActivationError> {
        let pipe = connect_client(&self.pipe_name)?;
        write_all(pipe.raw, frame)
    }

    #[doc(hidden)]
    pub fn stall_connection_for_test(&self) -> Result<(), WindowsActivationError> {
        let _pipe = connect_client(&self.pipe_name)?;
        thread::sleep(IO_TIMEOUT + Duration::from_millis(100));
        Ok(())
    }

    fn shutdown_inner(&mut self) -> Result<(), WindowsActivationError> {
        if self.join.is_none() {
            return Ok(());
        }
        self.stop.store(true, Ordering::Release);
        let result = self
            .join
            .take()
            .map(|join| {
                join.join()
                    .map_err(|_| WindowsActivationError::ListenerPanicked)
            })
            .transpose()
            .map(|_| ());
        self.alive.store(false, Ordering::Release);
        result
    }
}

impl Drop for WindowsInstanceGuard {
    fn drop(&mut self) {
        let _ = self.shutdown_inner();
        let _ = &self.mutex;
    }
}

pub fn acquire_or_activate(
    product_identity: &str,
    command: ActivationCommand,
    shell_commands: ShellCommandSender,
) -> Result<WindowsInstanceRole, WindowsActivationError> {
    let sid = current_token_sid()?;
    let names = instance_names(product_identity, &sid)?;
    let mutex_name = wide_nul(&names.mutex);
    // CreateMutexW only promises to set ERROR_ALREADY_EXISTS for the existing-object case. Clear
    // stale thread-local error state so a newly created mutex cannot be mistaken for a secondary.
    unsafe { SetLastError(ERROR_SUCCESS) };
    let mutex = unsafe { CreateMutexW(None, false, PCWSTR(mutex_name.as_ptr())) }
        .map(OwnedHandle::new)
        .map_err(|error| native("create per-user instance mutex", error))?;
    let already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;

    if already_exists {
        let envelope = ActivationEnvelope::new(command, current_process_identity()?);
        let acknowledgement = send_activation(&names.pipe, &envelope)?;
        drop(mutex);
        return Ok(WindowsInstanceRole::Secondary(acknowledgement));
    }

    start_primary(mutex, names.pipe, sid, shell_commands).map(WindowsInstanceRole::Primary)
}

pub fn instance_names(
    product_identity: &str,
    sid: &[u8],
) -> Result<WindowsInstanceNames, WindowsActivationError> {
    if product_identity.is_empty() {
        return Err(WindowsActivationError::EmptyProductIdentity);
    }
    if product_identity.len() > 64 {
        return Err(WindowsActivationError::ProductIdentityTooLong);
    }
    if !product_identity
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return Err(WindowsActivationError::InvalidProductIdentity);
    }
    let sid_hex: String = sid.iter().map(|byte| format!("{byte:02X}")).collect();
    let stem = format!("{product_identity}.{sid_hex}");
    Ok(WindowsInstanceNames {
        mutex: format!(r"Global\{stem}.instance"),
        pipe: format!(r"\\.\pipe\{stem}.activation"),
    })
}

fn start_primary(
    mutex: OwnedHandle,
    pipe_name: String,
    sid: Vec<u8>,
    shell_commands: ShellCommandSender,
) -> Result<WindowsInstanceGuard, WindowsActivationError> {
    let stop = Arc::new(AtomicBool::new(false));
    let alive = Arc::new(AtomicBool::new(false));
    let accepted = Arc::new(AtomicU64::new(0));
    let rejected = Arc::new(AtomicU64::new(0));
    let (startup_tx, startup_rx) = mpsc::sync_channel(1);
    let thread_pipe = pipe_name.clone();
    let thread_stop = Arc::clone(&stop);
    let thread_alive = Arc::clone(&alive);
    let thread_accepted = Arc::clone(&accepted);
    let thread_rejected = Arc::clone(&rejected);
    let join = thread::Builder::new()
        .name("clipline-instance-activation".into())
        .spawn(move || {
            listener_loop(
                &thread_pipe,
                &sid,
                shell_commands,
                &thread_stop,
                &thread_alive,
                &thread_accepted,
                &thread_rejected,
                startup_tx,
            );
        })
        .map_err(|error| WindowsActivationError::ListenerStartup(error.to_string()))?;

    match startup_rx.recv_timeout(CONNECT_TIMEOUT) {
        Ok(Ok(())) => Ok(WindowsInstanceGuard {
            mutex,
            pipe_name,
            stop,
            alive,
            accepted,
            rejected,
            join: Some(join),
        }),
        Ok(Err(error)) => {
            let _ = join.join();
            Err(WindowsActivationError::ListenerStartup(error))
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            stop.store(true, Ordering::Release);
            let _ = join.join();
            Err(WindowsActivationError::ListenerStartupTimeout)
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let _ = join.join();
            Err(WindowsActivationError::ListenerStartup(
                "listener exited before readiness".into(),
            ))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn listener_loop(
    pipe_name: &str,
    primary_sid: &[u8],
    shell_commands: ShellCommandSender,
    stop: &AtomicBool,
    alive: &AtomicBool,
    accepted: &AtomicU64,
    rejected: &AtomicU64,
    startup: mpsc::SyncSender<Result<(), String>>,
) {
    let mut startup = Some(startup);
    alive.store(true, Ordering::Release);
    loop {
        let pipe = match create_server_pipe(pipe_name) {
            Ok(pipe) => pipe,
            Err(error) => {
                if let Some(startup) = startup.take() {
                    let _ = startup.send(Err(error.to_string()));
                }
                break;
            }
        };
        if let Some(startup) = startup.take() {
            let _ = startup.send(Ok(()));
        }
        let connected = match connect_server(pipe.raw, stop) {
            Ok(connected) => connected,
            Err(_) => continue,
        };
        if !connected || stop.load(Ordering::Acquire) {
            break;
        }
        let result = handle_connection(pipe.raw, primary_sid, &shell_commands);
        let acknowledged = result.is_ok();
        let ack = if acknowledged {
            ACK_ACCEPTED
        } else {
            ACK_REJECTED
        };
        let _ = write_all(pipe.raw, &[ack]);
        if acknowledged {
            accepted.fetch_add(1, Ordering::Relaxed);
        } else {
            rejected.fetch_add(1, Ordering::Relaxed);
        }
    }
    alive.store(false, Ordering::Release);
}

fn create_server_pipe(pipe_name: &str) -> Result<OwnedHandle, WindowsActivationError> {
    let pipe_name = wide_nul(pipe_name);
    let handle = unsafe {
        CreateNamedPipeW(
            PCWSTR(pipe_name.as_ptr()),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            PIPE_BUFFER_BYTES,
            PIPE_BUFFER_BYTES,
            IO_TIMEOUT.as_millis() as u32,
            None,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        Err(last_native("create local activation pipe"))
    } else {
        Ok(OwnedHandle::new(handle))
    }
}

fn connect_server(pipe: HANDLE, stop: &AtomicBool) -> Result<bool, WindowsActivationError> {
    loop {
        match unsafe { ConnectNamedPipe(pipe, None) } {
            Ok(()) => return Ok(true),
            Err(error) if error.code() == HRESULT::from_win32(ERROR_PIPE_CONNECTED.0) => {
                return Ok(true);
            }
            Err(error) if error.code() == HRESULT::from_win32(ERROR_PIPE_LISTENING.0) => {
                if stop.load(Ordering::Acquire) {
                    return Ok(false);
                }
                thread::sleep(POLL_INTERVAL);
            }
            // A client connected and closed before the server observed it. Let the normal
            // connection handler classify and count the incomplete request, then recreate the
            // one-instance pipe.
            Err(error) if error.code() == HRESULT::from_win32(ERROR_NO_DATA.0) => return Ok(true),
            Err(error) => return Err(native("accept activation pipe client", error)),
        }
    }
}

fn handle_connection(
    pipe: HANDLE,
    primary_sid: &[u8],
    shell_commands: &ShellCommandSender,
) -> Result<(), WindowsActivationError> {
    let client_process_id = named_pipe_client_process_id(pipe)?;
    // Windows requires at least one pipe read before impersonation. Keep the bytes opaque until
    // after the peer SID and process instance have been authenticated.
    let payload = read_frame(pipe, Instant::now() + IO_TIMEOUT)?;
    let client_sid = impersonated_client_sid(pipe)?;
    if client_sid != primary_sid {
        return Err(WindowsActivationError::Protocol(
            "activation peer SID mismatch".into(),
        ));
    }
    let observed_process = process_identity(client_process_id)?;
    let envelope = ActivationEnvelope::decode(&payload)
        .map_err(|error| WindowsActivationError::Protocol(error.to_string()))?;
    validate_activation_peer(
        &ActivationPeer {
            sid: primary_sid.to_vec(),
            process: envelope.client(),
        },
        &ActivationPeer {
            sid: client_sid,
            process: observed_process,
        },
    )
    .map_err(|error| WindowsActivationError::Protocol(error.to_string()))?;
    if let Some(command) = envelope.shell_command() {
        shell_commands
            .try_send(command)
            .map_err(|error| WindowsActivationError::Queue(error.to_string()))?;
    }
    Ok(())
}

fn send_activation(
    pipe_name: &str,
    envelope: &ActivationEnvelope,
) -> Result<ActivationAcknowledgement, WindowsActivationError> {
    let payload = envelope
        .encode()
        .map_err(|error| WindowsActivationError::Protocol(error.to_string()))?;
    let length = u32::try_from(payload.len())
        .expect("activation payload bound fits u32")
        .to_le_bytes();
    let mut frame = Vec::with_capacity(payload.len() + length.len());
    frame.extend_from_slice(&length);
    frame.extend_from_slice(&payload);
    let pipe = connect_client(pipe_name)?;
    write_all(pipe.raw, &frame)?;
    if !read_ack(pipe.raw)? {
        return Err(WindowsActivationError::Rejected);
    }
    Ok(match envelope.command() {
        ActivationCommand::Reveal => ActivationAcknowledgement::RevealQueued,
        ActivationCommand::AutostartNoop => ActivationAcknowledgement::AutostartAcknowledged,
    })
}

fn connect_client(pipe_name: &str) -> Result<OwnedHandle, WindowsActivationError> {
    let pipe_name = wide_nul(pipe_name);
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        if unsafe { WaitNamedPipeW(PCWSTR(pipe_name.as_ptr()), 50) }.as_bool() {
            match unsafe {
                CreateFileW(
                    PCWSTR(pipe_name.as_ptr()),
                    GENERIC_READ.0 | GENERIC_WRITE.0,
                    FILE_SHARE_MODE(0),
                    None,
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    None,
                )
            } {
                Ok(handle) => return Ok(OwnedHandle::new(handle)),
                Err(error) if Instant::now() < deadline => {
                    let _ = error;
                }
                Err(error) => return Err(native("connect activation pipe", error)),
            }
        }
        if Instant::now() >= deadline {
            return Err(WindowsActivationError::Timeout);
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn read_frame(pipe: HANDLE, deadline: Instant) -> Result<Vec<u8>, WindowsActivationError> {
    let mut length = [0_u8; 4];
    read_exact_poll(pipe, &mut length, deadline)?;
    let length = usize::try_from(u32::from_le_bytes(length)).expect("u32 fits usize");
    if length > MAX_ACTIVATION_PAYLOAD_BYTES {
        return Err(WindowsActivationError::Protocol(format!(
            "activation payload is {length} bytes; maximum is {MAX_ACTIVATION_PAYLOAD_BYTES}"
        )));
    }
    let mut payload = vec![0_u8; length];
    read_exact_poll(pipe, &mut payload, deadline)?;
    Ok(payload)
}

fn read_ack(pipe: HANDLE) -> Result<bool, WindowsActivationError> {
    let mut ack = [0_u8; 1];
    read_exact_poll(pipe, &mut ack, Instant::now() + IO_TIMEOUT)?;
    match ack[0] {
        ACK_ACCEPTED => Ok(true),
        ACK_REJECTED => Ok(false),
        value => Err(WindowsActivationError::Protocol(format!(
            "invalid activation acknowledgement {value}"
        ))),
    }
}

fn read_exact_poll(
    pipe: HANDLE,
    buffer: &mut [u8],
    deadline: Instant,
) -> Result<(), WindowsActivationError> {
    let mut offset = 0;
    while offset < buffer.len() {
        let mut available = 0_u32;
        unsafe { PeekNamedPipe(pipe, None, 0, None, Some(&mut available), None) }
            .map_err(|error| native("poll activation pipe", error))?;
        if available == 0 {
            if Instant::now() >= deadline {
                return Err(WindowsActivationError::Timeout);
            }
            thread::sleep(POLL_INTERVAL);
            continue;
        }
        let count = usize::try_from(available)
            .unwrap_or(usize::MAX)
            .min(buffer.len() - offset);
        let mut read = 0_u32;
        unsafe {
            ReadFile(
                pipe,
                Some(&mut buffer[offset..offset + count]),
                Some(&mut read),
                None,
            )
        }
        .map_err(|error| native("read activation pipe", error))?;
        if read == 0 {
            return Err(WindowsActivationError::IncompleteFrame);
        }
        offset += usize::try_from(read).expect("read count fits usize");
    }
    Ok(())
}

fn write_all(pipe: HANDLE, buffer: &[u8]) -> Result<(), WindowsActivationError> {
    let mut offset = 0;
    while offset < buffer.len() {
        let mut written = 0_u32;
        unsafe { WriteFile(pipe, Some(&buffer[offset..]), Some(&mut written), None) }
            .map_err(|error| native("write activation pipe", error))?;
        if written == 0 {
            return Err(WindowsActivationError::IncompleteFrame);
        }
        offset += usize::try_from(written).expect("write count fits usize");
    }
    Ok(())
}

fn named_pipe_client_process_id(pipe: HANDLE) -> Result<u32, WindowsActivationError> {
    let mut process_id = 0_u32;
    unsafe { GetNamedPipeClientProcessId(pipe, &mut process_id) }
        .map_err(|error| native("query activation client process", error))?;
    if process_id == 0 {
        Err(WindowsActivationError::Protocol(
            "activation client reported process ID zero".into(),
        ))
    } else {
        Ok(process_id)
    }
}

fn impersonated_client_sid(pipe: HANDLE) -> Result<Vec<u8>, WindowsActivationError> {
    unsafe { ImpersonateNamedPipeClient(pipe) }
        .map_err(|error| native("impersonate activation client", error))?;
    let _revert = RevertGuard;
    let mut token = HANDLE::default();
    unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, true, &mut token) }
        .map_err(|error| native("open activation client token", error))?;
    token_sid(OwnedHandle::new(token).raw)
}

fn current_token_sid() -> Result<Vec<u8>, WindowsActivationError> {
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .map_err(|error| native("open current process token", error))?;
    token_sid(OwnedHandle::new(token).raw)
}

fn token_sid(token: HANDLE) -> Result<Vec<u8>, WindowsActivationError> {
    let mut required = 0_u32;
    let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut required) };
    if required == 0 {
        return Err(last_native("size token user information"));
    }
    let word = std::mem::size_of::<usize>();
    let words = usize::try_from(required)
        .expect("token size fits usize")
        .div_ceil(word);
    let mut buffer = vec![0_usize; words];
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            required,
            &mut required,
        )
    }
    .map_err(|error| native("read token user information", error))?;
    let token_user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    if !unsafe { IsValidSid(token_user.User.Sid) }.as_bool() {
        return Err(WindowsActivationError::Protocol(
            "token contained an invalid SID".into(),
        ));
    }
    let length = usize::try_from(unsafe { GetLengthSid(token_user.User.Sid) })
        .expect("SID length fits usize");
    let bytes = unsafe { std::slice::from_raw_parts(token_user.User.Sid.0.cast::<u8>(), length) };
    Ok(bytes.to_vec())
}

fn current_process_identity() -> Result<ProcessIdentity, WindowsActivationError> {
    let process_id = unsafe { GetCurrentProcessId() };
    process_identity_from_handle(process_id, unsafe { GetCurrentProcess() })
}

fn process_identity(process_id: u32) -> Result<ProcessIdentity, WindowsActivationError> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }
        .map(OwnedHandle::new)
        .map_err(|error| native("open activation client process", error))?;
    process_identity_from_handle(process_id, process.raw)
}

fn process_identity_from_handle(
    process_id: u32,
    process: HANDLE,
) -> Result<ProcessIdentity, WindowsActivationError> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) }
        .map_err(|error| native("query activation process creation time", error))?;
    let creation_time =
        (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
    ProcessIdentity::new(process_id, creation_time)
        .map_err(|error| WindowsActivationError::Protocol(error.to_string()))
}

fn wide_nul(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(Some(0)).collect()
}

fn native(operation: &'static str, error: windows::core::Error) -> WindowsActivationError {
    WindowsActivationError::Native {
        operation,
        message: error.to_string(),
    }
}

fn last_native(operation: &'static str) -> WindowsActivationError {
    WindowsActivationError::Native {
        operation,
        message: format!("Windows error {}", unsafe { GetLastError() }.0),
    }
}

struct OwnedHandle {
    raw: HANDLE,
}

impl OwnedHandle {
    const fn new(raw: HANDLE) -> Self {
        Self { raw }
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.raw) };
    }
}

struct RevertGuard;

impl Drop for RevertGuard {
    fn drop(&mut self) {
        let _ = unsafe { RevertToSelf() };
    }
}
