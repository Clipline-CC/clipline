use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use thiserror::Error;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, PeekMessageW, PostThreadMessageW,
    SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT,
    LLKHF_ALTDOWN, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_HOTKEY,
    WM_KEYDOWN, WM_KEYUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
    WM_USER, WM_XBUTTONDOWN, WM_XBUTTONUP, XBUTTON1, XBUTTON2,
};

use crate::hotkey::{
    replace_hotkeys, HotkeyKey, HotkeyRegistrationBackend, HotkeyReplacementOutcome, HotkeySet,
    HotkeySpec, HotkeyTriggerGate,
};
use crate::{ShellCommand, ShellCommandSender};

const COMMAND_CAPACITY: usize = 8;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const DUPLICATE_WINDOW_MS: u64 = 150;
const SERVICE_MESSAGE: u32 = WM_USER + 0x43;
const FIRST_REGISTRATION_ID: i32 = 0x4340;
const LAST_REGISTRATION_ID: i32 = 0xBFFF;
const VK_SHIFT: i32 = 0x10;
const VK_CONTROL: i32 = 0x11;
const VK_ALT: i32 = 0x12;

fn callback_slot() -> &'static Mutex<Option<Weak<CallbackState>>> {
    static CALLBACK: OnceLock<Mutex<Option<Weak<CallbackState>>>> = OnceLock::new();
    CALLBACK.get_or_init(|| Mutex::new(None))
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WindowsHotkeyServiceError {
    #[error("another Clipline hotkey service is already running")]
    AlreadyRunning,
    #[error("spawn hotkey service thread: {0}")]
    Spawn(String),
    #[error("hotkey service startup timed out")]
    StartupTimeout,
    #[error("hotkey service startup failed: {0}")]
    Startup(String),
    #[error("hotkey service command queue is full")]
    Busy,
    #[error("hotkey service is disconnected")]
    Disconnected,
    #[error("notify hotkey service thread: {0}")]
    Notify(String),
    #[error("hotkey replacement failed: {0}")]
    Replacement(String),
    #[error("hotkey service response timed out")]
    ResponseTimeout,
    #[error("hotkey service thread panicked")]
    ThreadPanicked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsHotkeyServiceSnapshot {
    pub thread_id: u32,
    pub active_labels: Vec<String>,
    pub keyboard_hook_installed: bool,
    pub mouse_hook_installed: bool,
    pub dropped_triggers: u64,
}

pub struct WindowsHotkeyService {
    command_tx: SyncSender<ServiceCommand>,
    thread_id: u32,
    join: Mutex<Option<JoinHandle<()>>>,
    callback: Arc<CallbackState>,
    keyboard_hook_installed: Arc<AtomicBool>,
    mouse_hook_installed: Arc<AtomicBool>,
    startup_warnings: Vec<String>,
}

struct Startup {
    thread_id: u32,
    warnings: Vec<String>,
}

enum ServiceCommand {
    Replace {
        candidate: HotkeySet,
        response: mpsc::Sender<Result<HotkeyReplacementOutcome, String>>,
    },
}

struct CallbackState {
    active: Mutex<HotkeySet>,
    gate: Mutex<HotkeyTriggerGate>,
    shell_commands: ShellCommandSender,
    started_at: Instant,
    dropped_triggers: AtomicU64,
}

impl CallbackState {
    fn handle_hook_key_down(&self, virtual_key: u32, alt_flag: bool) {
        let modifiers = current_modifiers(alt_flag);
        let matches = self.matches(virtual_key, modifiers);
        let deliver = self.gate.lock().is_ok_and(|mut gate| {
            gate.observe_hook_key_down_if(virtual_key, self.now_ms(), matches)
        });
        if matches && deliver {
            self.publish_trigger();
        }
    }

    fn handle_registered(&self, virtual_key: u32, modifiers: u32) {
        let matches = self.matches(virtual_key, modifiers);
        if matches
            && self
                .gate
                .lock()
                .is_ok_and(|mut gate| gate.observe_registered(virtual_key, self.now_ms()))
        {
            self.publish_trigger();
        }
    }

    fn handle_key_up(&self, virtual_key: u32) {
        if let Ok(mut gate) = self.gate.lock() {
            gate.observe_key_up(virtual_key);
        }
    }

    fn matches(&self, virtual_key: u32, modifiers: u32) -> bool {
        self.active.lock().is_ok_and(|active| {
            active.as_slice().iter().any(|hotkey| {
                hotkey.key.virtual_key_code() == virtual_key && hotkey.modifier_flags() == modifiers
            })
        })
    }

    fn publish_trigger(&self) {
        if self
            .shell_commands
            .try_send(ShellCommand::SaveReplay)
            .is_err()
        {
            self.dropped_triggers.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

impl WindowsHotkeyService {
    pub fn start(shell_commands: ShellCommandSender) -> Result<Self, WindowsHotkeyServiceError> {
        {
            let slot = callback_slot()
                .lock()
                .map_err(|_| WindowsHotkeyServiceError::AlreadyRunning)?;
            if slot.as_ref().and_then(Weak::upgrade).is_some() {
                return Err(WindowsHotkeyServiceError::AlreadyRunning);
            }
        }

        let empty = HotkeySet::parse(&[]).expect("empty hotkey set is valid");
        let callback = Arc::new(CallbackState {
            active: Mutex::new(empty),
            gate: Mutex::new(HotkeyTriggerGate::new(DUPLICATE_WINDOW_MS)),
            shell_commands,
            started_at: Instant::now(),
            dropped_triggers: AtomicU64::new(0),
        });
        let keyboard_hook_installed = Arc::new(AtomicBool::new(false));
        let mouse_hook_installed = Arc::new(AtomicBool::new(false));
        let (command_tx, command_rx) = mpsc::sync_channel(COMMAND_CAPACITY);
        // A rendezvous channel prevents a startup-timeout race from leaving a
        // detached service thread alive after the receiver has returned.
        let (ready_tx, ready_rx) = mpsc::sync_channel(0);
        let thread_callback = Arc::clone(&callback);
        let thread_keyboard = Arc::clone(&keyboard_hook_installed);
        let thread_mouse = Arc::clone(&mouse_hook_installed);
        let join = thread::Builder::new()
            .name("clipline-hotkey-service".into())
            .spawn(move || {
                run_service_thread(
                    command_rx,
                    ready_tx,
                    thread_callback,
                    thread_keyboard,
                    thread_mouse,
                );
            })
            .map_err(|error| WindowsHotkeyServiceError::Spawn(error.to_string()))?;

        let startup = match ready_rx.recv_timeout(COMMAND_TIMEOUT) {
            Ok(Ok(startup)) => startup,
            Ok(Err(error)) => {
                let _ = join.join();
                return Err(WindowsHotkeyServiceError::Startup(error));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(WindowsHotkeyServiceError::StartupTimeout)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = join.join();
                return Err(WindowsHotkeyServiceError::Disconnected);
            }
        };

        Ok(Self {
            command_tx,
            thread_id: startup.thread_id,
            join: Mutex::new(Some(join)),
            callback,
            keyboard_hook_installed,
            mouse_hook_installed,
            startup_warnings: startup.warnings,
        })
    }

    pub fn replace(
        &self,
        candidate: &HotkeySet,
    ) -> Result<HotkeyReplacementOutcome, WindowsHotkeyServiceError> {
        let (response_tx, response_rx) = mpsc::channel();
        self.command_tx
            .try_send(ServiceCommand::Replace {
                candidate: candidate.clone(),
                response: response_tx,
            })
            .map_err(|error| match error {
                TrySendError::Full(_) => WindowsHotkeyServiceError::Busy,
                TrySendError::Disconnected(_) => WindowsHotkeyServiceError::Disconnected,
            })?;
        // SAFETY: the thread id came from this service's initialized message
        // queue, and the message carries no pointers or borrowed data.
        unsafe { PostThreadMessageW(self.thread_id, SERVICE_MESSAGE, WPARAM(0), LPARAM(0)) }
            .map_err(|error| WindowsHotkeyServiceError::Notify(error.to_string()))?;
        match response_rx.recv_timeout(COMMAND_TIMEOUT) {
            Ok(Ok(outcome)) => Ok(outcome),
            Ok(Err(error)) => Err(WindowsHotkeyServiceError::Replacement(error)),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(WindowsHotkeyServiceError::ResponseTimeout),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(WindowsHotkeyServiceError::Disconnected)
            }
        }
    }

    #[must_use]
    pub fn startup_warnings(&self) -> &[String] {
        &self.startup_warnings
    }

    #[must_use]
    pub fn snapshot(&self) -> WindowsHotkeyServiceSnapshot {
        WindowsHotkeyServiceSnapshot {
            thread_id: self.thread_id,
            active_labels: self
                .callback
                .active
                .lock()
                .map_or_else(|_| Vec::new(), |active| active.labels()),
            keyboard_hook_installed: self.keyboard_hook_installed.load(Ordering::Acquire),
            mouse_hook_installed: self.mouse_hook_installed.load(Ordering::Acquire),
            dropped_triggers: self.callback.dropped_triggers.load(Ordering::Relaxed),
        }
    }

    /// Posts a registered-hotkey notification through the real service
    /// message queue. This is a deterministic device-test seam; it does not
    /// synthesize user input.
    #[doc(hidden)]
    pub fn post_registered_for_test(
        &self,
        hotkey: &HotkeySpec,
    ) -> Result<(), WindowsHotkeyServiceError> {
        let packed = (isize::try_from(hotkey.key.virtual_key_code()).unwrap_or(isize::MAX) << 16)
            | isize::try_from(hotkey.modifier_flags()).unwrap_or(isize::MAX);
        // SAFETY: the service owns the target queue and WM_HOTKEY lparam is
        // the documented pair of modifier/vk integer fields.
        unsafe { PostThreadMessageW(self.thread_id, WM_HOTKEY, WPARAM(0), LPARAM(packed)) }
            .map_err(|error| WindowsHotkeyServiceError::Notify(error.to_string()))
    }

    pub fn shutdown(mut self) -> Result<(), WindowsHotkeyServiceError> {
        self.stop_and_join()
    }

    fn stop_and_join(&mut self) -> Result<(), WindowsHotkeyServiceError> {
        let Some(join) = self.join.get_mut().ok().and_then(Option::take) else {
            return Ok(());
        };
        // SAFETY: this posts a value-only quit message to the owned queue.
        let post_result =
            unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        let join_result = join.join();
        if let Err(error) = post_result {
            return Err(WindowsHotkeyServiceError::Notify(error.to_string()));
        }
        join_result.map_err(|_| WindowsHotkeyServiceError::ThreadPanicked)
    }
}

impl Drop for WindowsHotkeyService {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

fn run_service_thread(
    command_rx: Receiver<ServiceCommand>,
    ready_tx: SyncSender<Result<Startup, String>>,
    callback: Arc<CallbackState>,
    keyboard_hook_installed: Arc<AtomicBool>,
    mouse_hook_installed: Arc<AtomicBool>,
) {
    let mut message = MSG::default();
    // SAFETY: PeekMessage initializes this thread's queue. The MSG pointer is
    // valid for the duration of the call and no message is removed.
    unsafe {
        let _ = PeekMessageW(&mut message, None, WM_USER, WM_USER, PM_NOREMOVE);
    }
    {
        let Ok(mut slot) = callback_slot().lock() else {
            let _ = ready_tx.send(Err("hotkey callback lock poisoned".into()));
            return;
        };
        if slot.as_ref().and_then(Weak::upgrade).is_some() {
            let _ = ready_tx.send(Err(
                "another Clipline hotkey service is already running".into()
            ));
            return;
        }
        *slot = Some(Arc::downgrade(&callback));
    }

    let mut warnings = Vec::new();
    // SAFETY: the callback has the required ABI and remains in the binary for
    // the hook lifetime. A low-level hook does not require a module handle.
    let keyboard_hook =
        match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), None, 0) } {
            Ok(hook) => {
                keyboard_hook_installed.store(true, Ordering::Release);
                Some(hook)
            }
            Err(_) => {
                warnings.push("low-level save keyboard hotkey hook could not be installed".into());
                None
            }
        };
    // SAFETY: no precondition beyond being in the current process.
    let thread_id = unsafe { GetCurrentThreadId() };
    if ready_tx
        .send(Ok(Startup {
            thread_id,
            warnings,
        }))
        .is_err()
    {
        cleanup_callback_and_hooks(
            keyboard_hook,
            None,
            &keyboard_hook_installed,
            &mouse_hook_installed,
        );
        return;
    }

    let mut state = ThreadState {
        callback,
        registrations: BTreeMap::new(),
        active: HotkeySet::parse(&[]).expect("empty hotkey set is valid"),
        next_registration_id: FIRST_REGISTRATION_ID,
        mouse_hook: None,
        mouse_hook_installed: Arc::clone(&mouse_hook_installed),
    };

    loop {
        // SAFETY: this thread owns the queue and MSG storage.
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) }.0;
        if result <= 0 {
            break;
        }
        match message.message {
            SERVICE_MESSAGE => drain_commands(&command_rx, &mut state),
            WM_HOTKEY => {
                let packed = message.lParam.0 as usize;
                let modifiers = (packed & 0xffff) as u32;
                let virtual_key = ((packed >> 16) & 0xffff) as u32;
                state.callback.handle_registered(virtual_key, modifiers);
            }
            _ => {
                // SAFETY: MSG was obtained from GetMessage on this thread.
                unsafe {
                    let _ = TranslateMessage(&message);
                    let _ = DispatchMessageW(&message);
                }
            }
        }
    }

    state.unregister_all();
    cleanup_callback_and_hooks(
        keyboard_hook,
        state.mouse_hook.take(),
        &keyboard_hook_installed,
        &mouse_hook_installed,
    );
}

fn drain_commands(command_rx: &Receiver<ServiceCommand>, state: &mut ThreadState) {
    while let Ok(ServiceCommand::Replace {
        candidate,
        response,
    }) = command_rx.try_recv()
    {
        let result = state.replace(candidate);
        let _ = response.send(result);
    }
}

struct ThreadState {
    callback: Arc<CallbackState>,
    registrations: BTreeMap<HotkeySpec, Option<i32>>,
    active: HotkeySet,
    next_registration_id: i32,
    mouse_hook: Option<HHOOK>,
    mouse_hook_installed: Arc<AtomicBool>,
}

impl ThreadState {
    fn replace(&mut self, candidate: HotkeySet) -> Result<HotkeyReplacementOutcome, String> {
        let old_needs_mouse = set_needs_mouse(&self.active);
        let new_needs_mouse = set_needs_mouse(&candidate);
        let installed_mouse_for_candidate = if new_needs_mouse && self.mouse_hook.is_none() {
            // SAFETY: callback ABI/lifetime is process-static, as for keyboard.
            let hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), None, 0) }
                .map_err(|_| {
                    "low-level save mouse hotkey hook could not be installed".to_string()
                })?;
            self.mouse_hook = Some(hook);
            self.mouse_hook_installed.store(true, Ordering::Release);
            true
        } else {
            false
        };

        let old = self.active.clone();
        let result = replace_hotkeys(&old, &candidate, self);
        match result {
            Ok(outcome) => {
                self.active = candidate.clone();
                if let Ok(mut active) = self.callback.active.lock() {
                    *active = candidate;
                }
                if old_needs_mouse && !new_needs_mouse {
                    self.drop_mouse_hook();
                }
                Ok(outcome)
            }
            Err(error) => {
                if installed_mouse_for_candidate && !old_needs_mouse {
                    self.drop_mouse_hook();
                }
                Err(error.to_string())
            }
        }
    }

    fn drop_mouse_hook(&mut self) {
        if let Some(hook) = self.mouse_hook.take() {
            // SAFETY: this thread owns the live hook handle.
            let _ = unsafe { UnhookWindowsHookEx(hook) };
        }
        self.mouse_hook_installed.store(false, Ordering::Release);
    }

    fn unregister_all(&mut self) {
        for registration_id in self.registrations.values().flatten() {
            // SAFETY: registration ids belong to this thread and no HWND.
            let _ = unsafe { UnregisterHotKey(None, *registration_id) };
        }
        self.registrations.clear();
    }
}

impl HotkeyRegistrationBackend for ThreadState {
    type Error = String;

    fn is_registered(&self, hotkey: &HotkeySpec) -> bool {
        self.registrations.contains_key(hotkey)
    }

    fn register(&mut self, hotkey: &HotkeySpec) -> Result<(), Self::Error> {
        if self.registrations.contains_key(hotkey) {
            return Ok(());
        }
        if matches!(
            hotkey.key,
            HotkeyKey::Middle | HotkeyKey::Mouse4 | HotkeyKey::Mouse5
        ) {
            self.registrations.insert(hotkey.clone(), None);
            return Ok(());
        }
        let registration_id = self.next_registration_id;
        if registration_id > LAST_REGISTRATION_ID {
            return Err("hotkey registration id exhausted".into());
        }
        self.next_registration_id = self
            .next_registration_id
            .checked_add(1)
            .ok_or_else(|| "hotkey registration id exhausted".to_string())?;
        // SAFETY: registration is thread-scoped (no HWND), the id is unique
        // in this service, and vk/modifier fields come from the closed parser.
        unsafe {
            RegisterHotKey(
                None,
                registration_id,
                HOT_KEY_MODIFIERS(hotkey.registration_modifier_flags()),
                hotkey.key.virtual_key_code(),
            )
        }
        .map_err(|error| error.to_string())?;
        self.registrations
            .insert(hotkey.clone(), Some(registration_id));
        Ok(())
    }

    fn unregister(&mut self, hotkey: &HotkeySpec) -> Result<(), Self::Error> {
        let Some(registration_id) = self.registrations.get(hotkey).copied() else {
            return Ok(());
        };
        if let Some(registration_id) = registration_id {
            // SAFETY: registration id belongs to this thread and no HWND.
            unsafe { UnregisterHotKey(None, registration_id) }
                .map_err(|error| error.to_string())?;
        }
        self.registrations.remove(hotkey);
        Ok(())
    }
}

fn set_needs_mouse(set: &HotkeySet) -> bool {
    set.as_slice().iter().any(|hotkey| {
        matches!(
            hotkey.key,
            HotkeyKey::Middle | HotkeyKey::Mouse4 | HotkeyKey::Mouse5
        )
    })
}

fn cleanup_callback_and_hooks(
    keyboard_hook: Option<HHOOK>,
    mouse_hook: Option<HHOOK>,
    keyboard_hook_installed: &AtomicBool,
    mouse_hook_installed: &AtomicBool,
) {
    if let Some(mouse_hook) = mouse_hook {
        // SAFETY: cleanup runs on the thread that installed the hook.
        let _ = unsafe { UnhookWindowsHookEx(mouse_hook) };
    }
    if let Some(keyboard_hook) = keyboard_hook {
        // SAFETY: cleanup runs on the thread that installed the hook.
        let _ = unsafe { UnhookWindowsHookEx(keyboard_hook) };
    }
    keyboard_hook_installed.store(false, Ordering::Release);
    mouse_hook_installed.store(false, Ordering::Release);
    if let Ok(mut slot) = callback_slot().lock() {
        *slot = None;
    }
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let message = wparam.0 as u32;
        // SAFETY: Windows supplies KBDLLHOOKSTRUCT for a low-level keyboard
        // hook whenever code is HC_ACTION.
        let keyboard = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        match message {
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                with_callback(|callback| {
                    callback.handle_hook_key_down(
                        keyboard.vkCode,
                        keyboard.flags.contains(LLKHF_ALTDOWN),
                    );
                });
            }
            WM_KEYUP | WM_SYSKEYUP => {
                with_callback(|callback| callback.handle_key_up(keyboard.vkCode));
            }
            _ => {}
        }
    }
    // SAFETY: forwarding is required by the hook contract; this service does
    // not consume input.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let message = wparam.0 as u32;
        match message {
            WM_MBUTTONDOWN => with_callback(|callback| callback.handle_hook_key_down(0x04, false)),
            WM_MBUTTONUP => with_callback(|callback| callback.handle_key_up(0x04)),
            WM_XBUTTONDOWN | WM_XBUTTONUP => {
                // SAFETY: Windows supplies MSLLHOOKSTRUCT for a low-level
                // mouse hook whenever code is HC_ACTION.
                let mouse = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
                if let Some(virtual_key) = xbutton_virtual_key(mouse.mouseData) {
                    if message == WM_XBUTTONDOWN {
                        with_callback(|callback| {
                            callback.handle_hook_key_down(virtual_key, false);
                        });
                    } else {
                        with_callback(|callback| callback.handle_key_up(virtual_key));
                    }
                }
            }
            _ => {}
        }
    }
    // SAFETY: forwarding is required by the hook contract.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn with_callback(operation: impl FnOnce(&CallbackState)) {
    let callback = callback_slot()
        .lock()
        .ok()
        .and_then(|slot| slot.as_ref().and_then(Weak::upgrade));
    if let Some(callback) = callback {
        operation(&callback);
    }
}

fn current_modifiers(alt_flag: bool) -> u32 {
    let mut modifiers = 0;
    // SAFETY: these are documented virtual-key constants and querying them
    // has no ownership or lifetime requirements.
    if unsafe { GetAsyncKeyState(VK_CONTROL) } < 0 {
        modifiers |= crate::hotkey::MODIFIER_CONTROL;
    }
    // SAFETY: see above.
    if unsafe { GetAsyncKeyState(VK_SHIFT) } < 0 {
        modifiers |= crate::hotkey::MODIFIER_SHIFT;
    }
    // SAFETY: see above.
    if alt_flag || unsafe { GetAsyncKeyState(VK_ALT) } < 0 {
        modifiers |= crate::hotkey::MODIFIER_ALT;
    }
    modifiers
}

fn xbutton_virtual_key(mouse_data: u32) -> Option<u32> {
    match ((mouse_data >> 16) & 0xffff) as u16 {
        XBUTTON1 => Some(0x05),
        XBUTTON2 => Some(0x06),
        _ => None,
    }
}
