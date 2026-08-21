//! Low-level Windows keyboard hook used as a fallback for games that do not
//! reliably deliver registered global shortcuts while focused.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Instant;

use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_MBUTTON, VK_XBUTTON1, VK_XBUTTON2,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, PeekMessageW, PostThreadMessageW,
    SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, HC_ACTION, KBDLLHOOKSTRUCT,
    LLKHF_ALTDOWN, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN,
    WM_KEYUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_USER,
    WM_XBUTTONDOWN, WM_XBUTTONUP, XBUTTON1, XBUTTON2,
};

const VK_SHIFT_CODE: i32 = 0x10;
const VK_CONTROL_CODE: i32 = 0x11;
const VK_ALT_CODE: i32 = 0x12;
const VK_MBUTTON_CODE: u32 = VK_MBUTTON as u32;
const VK_XBUTTON1_CODE: u32 = VK_XBUTTON1 as u32;
const VK_XBUTTON2_CODE: u32 = VK_XBUTTON2 as u32;
const VK_F1_CODE: u32 = 0x70;
const VK_F24_CODE: u32 = 0x87;

static HOTKEY_HOOK: OnceLock<Arc<HookState>> = OnceLock::new();
/// Set while a Settings/first-run hotkey field is focused so the live bind
/// cannot fire (or, for OS-registered F-keys, swallow) the key being captured.
static ACTIONS_PAUSED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HookAction {
    SaveReplay,
    ToggleRecording,
    Bookmark,
    ScreenshotRegion,
    ScreenshotScreen,
    ScreenshotWindow,
}

/// A matched hotkey press, with the moment the hook saw the key go down.
/// The stamp is taken inside the hook callback rather than by the dispatch
/// thread, so a descheduled or busy dispatcher cannot shift a time-sensitive
/// action — a bookmark — later than the press that asked for it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct HookTrigger {
    pub(crate) action: HookAction,
    pub(crate) pressed_at: Instant,
}

/// Every configured binding set, by action. A struct rather than positional
/// slices so adding an action stays a one-line change at each call site.
#[derive(Clone, Copy, Debug, Default)]
pub struct HookHotkeys<'a> {
    pub save: &'a [&'a str],
    pub recording: &'a [&'a str],
    pub bookmark: &'a [&'a str],
    pub screenshot_region: &'a [&'a str],
    pub screenshot_screen: &'a [&'a str],
    pub screenshot_window: &'a [&'a str],
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HookHotkey {
    ctrl: bool,
    alt: bool,
    shift: bool,
    key_vk: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HookBinding {
    hotkey: HookHotkey,
    action: HookAction,
}

struct HookState {
    bindings: Mutex<Vec<HookBinding>>,
    down_keys: Mutex<BTreeSet<u32>>,
    trigger_tx: Sender<HookTrigger>,
    keyboard_hook: Option<KeyboardHookThread>,
    mouse_hook: Mutex<Option<MouseHookThread>>,
    paused: AtomicBool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct KeyboardHookThread {
    thread_id: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MouseHookThread {
    thread_id: u32,
}

impl HookState {
    fn set_hotkeys(&self, hotkeys: HookHotkeys<'_>) -> Result<(), String> {
        let parsed = parse_hook_bindings(hotkeys)?;
        let requires_mouse_hook = parsed
            .iter()
            .any(|binding| binding.hotkey.requires_mouse_hook());
        if requires_mouse_hook {
            self.ensure_mouse_hook()?;
        }
        let mut bindings = self
            .bindings
            .lock()
            .map_err(|_| "hotkey lock poisoned".to_string())?;
        *bindings = parsed;
        drop(bindings);
        if !requires_mouse_hook {
            self.stop_mouse_hook();
        }
        Ok(())
    }

    fn on_key_down(&self, vk_code: u32, ctrl: bool, alt: bool, shift: bool) -> bool {
        if self.paused.load(Ordering::Acquire) {
            return false;
        }
        // Before any lock: contention here, and the dispatch thread's own
        // scheduling, must not move where a bookmark lands.
        let pressed_at = Instant::now();
        let mut down_keys = match self.down_keys.lock() {
            Ok(keys) => keys,
            Err(_) => return false,
        };
        if !down_keys.insert(vk_code) {
            return false;
        }
        drop(down_keys);

        let bindings = match self.bindings.lock() {
            Ok(bindings) => bindings,
            Err(_) => return false,
        };
        if let Some(action) = bindings
            .iter()
            .find(|binding| binding.hotkey.matches(vk_code, ctrl, alt, shift))
            .map(|binding| binding.action)
        {
            let _ = self.trigger_tx.send(HookTrigger { action, pressed_at });
            return true;
        }
        false
    }

    fn on_key_up(&self, vk_code: u32) {
        if let Ok(mut down_keys) = self.down_keys.try_lock() {
            down_keys.remove(&vk_code);
        }
    }

    fn ensure_mouse_hook(&self) -> Result<(), String> {
        let mut mouse_hook = self
            .mouse_hook
            .lock()
            .map_err(|_| "mouse hotkey hook lock poisoned".to_string())?;
        if mouse_hook.is_some() {
            return Ok(());
        }

        let (ready_tx, ready_rx) = mpsc::channel();
        thread::Builder::new()
            .name("clipline-mouse-hotkey-hook".into())
            .spawn(move || run_mouse_hook(ready_tx))
            .map_err(|e| format!("spawn mouse hotkey hook: {e}"))?;

        let thread_id = ready_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .map_err(|e| format!("install mouse hotkey hook: {e}"))??;
        *mouse_hook = Some(MouseHookThread { thread_id });
        Ok(())
    }

    fn stop_mouse_hook(&self) {
        let Ok(mut mouse_hook) = self.mouse_hook.lock() else {
            return;
        };
        if let Some(mouse_hook) = mouse_hook.take() {
            mouse_hook.stop();
        }
    }
}

impl Drop for HookState {
    fn drop(&mut self) {
        if let Some(keyboard_hook) = self.keyboard_hook.take() {
            keyboard_hook.stop();
        }
        self.stop_mouse_hook();
    }
}

impl HookHotkey {
    fn matches(&self, vk_code: u32, ctrl: bool, alt: bool, shift: bool) -> bool {
        self.key_vk == vk_code && self.ctrl == ctrl && self.alt == alt && self.shift == shift
    }

    fn requires_mouse_hook(&self) -> bool {
        matches!(
            self.key_vk,
            VK_MBUTTON_CODE | VK_XBUTTON1_CODE | VK_XBUTTON2_CODE
        )
    }
}

impl MouseHookThread {
    fn stop(self) {
        if unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) } == 0 {
            tracing::warn!(event = "mouse_hotkey_hook_stop_failed");
        }
    }
}

impl KeyboardHookThread {
    fn stop(self) {
        if unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) } == 0 {
            tracing::warn!(event = "keyboard_hotkey_hook_stop_failed");
        }
    }
}

pub fn install_hotkey_hook<F>(hotkeys: HookHotkeys<'_>, on_trigger: F) -> Result<(), String>
where
    F: Fn(HookTrigger) + Send + Sync + 'static,
{
    if let Some(state) = HOTKEY_HOOK.get() {
        return state.set_hotkeys(hotkeys);
    }

    let parsed_bindings = parse_hook_bindings(hotkeys)?;
    let (trigger_tx, trigger_rx) = mpsc::channel();
    thread::Builder::new()
        .name("clipline-hotkey-dispatch".into())
        .spawn(move || {
            while let Ok(trigger) = trigger_rx.recv() {
                on_trigger(trigger);
            }
        })
        .map_err(|e| format!("spawn hotkey dispatcher: {e}"))?;

    let requires_mouse_hook = parsed_bindings
        .iter()
        .any(|binding| binding.hotkey.requires_mouse_hook());
    let keyboard_hook = start_keyboard_hook()?;
    let state = Arc::new(HookState {
        bindings: Mutex::new(parsed_bindings),
        down_keys: Mutex::new(BTreeSet::new()),
        trigger_tx,
        keyboard_hook: Some(keyboard_hook),
        mouse_hook: Mutex::new(None),
        paused: AtomicBool::new(ACTIONS_PAUSED.load(Ordering::Acquire)),
    });
    if requires_mouse_hook {
        state.ensure_mouse_hook()?;
    }
    HOTKEY_HOOK
        .set(state)
        .map_err(|_| "hotkey hook was already installed".to_string())?;
    Ok(())
}

pub fn set_hotkeys(hotkeys: HookHotkeys<'_>) -> Result<(), String> {
    if let Some(state) = HOTKEY_HOOK.get() {
        state.set_hotkeys(hotkeys)
    } else {
        parse_hook_bindings(hotkeys).map(|_| ())
    }
}

pub fn set_actions_paused(paused: bool) {
    ACTIONS_PAUSED.store(paused, Ordering::Release);
    if let Some(state) = HOTKEY_HOOK.get() {
        state.paused.store(paused, Ordering::Release);
    }
}

pub fn actions_paused() -> bool {
    ACTIONS_PAUSED.load(Ordering::Acquire)
}

fn start_keyboard_hook() -> Result<KeyboardHookThread, String> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let (start_tx, start_rx) = mpsc::channel();
    thread::Builder::new()
        .name("clipline-keyboard-hotkey-hook".into())
        .spawn(move || run_keyboard_hook(ready_tx, start_rx))
        .map_err(|e| format!("spawn keyboard hotkey hook: {e}"))?;

    let keyboard_hook = wait_for_keyboard_hook_ready(ready_rx, std::time::Duration::from_secs(2))?;
    start_tx
        .send(())
        .map_err(|_| "start keyboard hotkey hook: hook thread exited".to_string())?;
    Ok(keyboard_hook)
}

fn wait_for_keyboard_hook_ready(
    ready_rx: Receiver<Result<u32, String>>,
    timeout: std::time::Duration,
) -> Result<KeyboardHookThread, String> {
    ready_rx
        .recv_timeout(timeout)
        .map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => "install keyboard hotkey hook: timed out".to_string(),
            mpsc::RecvTimeoutError::Disconnected => {
                "install keyboard hotkey hook: disconnected".to_string()
            }
        })?
        .map(|thread_id| KeyboardHookThread { thread_id })
}

fn parse_hook_hotkeys(raws: &[&str]) -> Result<Vec<HookHotkey>, String> {
    raws.iter().map(|raw| parse_hook_hotkey(raw)).collect()
}

fn parse_hook_bindings(hotkeys: HookHotkeys<'_>) -> Result<Vec<HookBinding>, String> {
    // Save Replay is always bound, so it is parsed strictly and claims its keys
    // first; the optional actions skip blanks and refuse to shadow a key that is
    // already taken.
    let mut bindings = parse_hook_hotkeys(hotkeys.save)?
        .into_iter()
        .map(|hotkey| HookBinding {
            hotkey,
            action: HookAction::SaveReplay,
        })
        .collect::<Vec<_>>();
    for (raws, action, label) in [
        (hotkeys.recording, HookAction::ToggleRecording, "recording"),
        (hotkeys.bookmark, HookAction::Bookmark, "bookmark"),
        (
            hotkeys.screenshot_region,
            HookAction::ScreenshotRegion,
            "screenshot region",
        ),
        (
            hotkeys.screenshot_screen,
            HookAction::ScreenshotScreen,
            "screenshot screen",
        ),
        (
            hotkeys.screenshot_window,
            HookAction::ScreenshotWindow,
            "screenshot window",
        ),
    ] {
        for raw in raws.iter().copied().filter(|raw| !raw.trim().is_empty()) {
            let hotkey = parse_hook_hotkey(raw)?;
            if bindings.iter().any(|binding| binding.hotkey == hotkey) {
                return Err(format!("{label} hotkey matches another configured hotkey"));
            }
            bindings.push(HookBinding { hotkey, action });
        }
    }
    Ok(bindings)
}

fn parse_hook_hotkey(raw: &str) -> Result<HookHotkey, String> {
    use crate::settings::hotkey::{parse_hotkey_spec, HotkeyKey};

    let spec = parse_hotkey_spec(raw)?;
    let key_vk = match &spec.key {
        HotkeyKey::Function(number) => VK_F1_CODE + u32::from(*number) - 1,
        HotkeyKey::Keyboard(key) => {
            keyboard_key_vk_code(key).ok_or_else(|| format!("unsupported hotkey key: {key}"))?
        }
        // PrintScreen is OS-registered via the global-shortcut plugin, never
        // routed through the low-level hook (the hook filter would also have to
        // swallow it OS-wide, which we do not want).
        HotkeyKey::PrintScreen => {
            return Err("PrintScreen is registered as a global shortcut, not a hook key".into())
        }
        HotkeyKey::Middle => VK_MBUTTON_CODE,
        HotkeyKey::Mouse4 => VK_XBUTTON1_CODE,
        HotkeyKey::Mouse5 => VK_XBUTTON2_CODE,
    };
    Ok(HookHotkey {
        ctrl: spec.ctrl,
        alt: spec.alt,
        shift: spec.shift,
        key_vk,
    })
}

fn run_keyboard_hook(ready_tx: Sender<Result<u32, String>>, start_rx: Receiver<()>) {
    let mut msg = unsafe { std::mem::zeroed::<MSG>() };
    unsafe {
        PeekMessageW(
            &mut msg,
            std::ptr::null_mut(),
            WM_USER,
            WM_USER,
            PM_NOREMOVE,
        );
    }
    let hook =
        unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), std::ptr::null_mut(), 0) };
    if hook.is_null() {
        let _ = ready_tx.send(Err(
            "low-level keyboard hotkey hook could not be installed".into(),
        ));
        return;
    }

    if ready_tx.send(Ok(unsafe { GetCurrentThreadId() })).is_err()
        || start_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .is_err()
    {
        unsafe {
            UnhookWindowsHookEx(hook);
        }
        return;
    }
    while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    unsafe {
        UnhookWindowsHookEx(hook);
    }
}

fn run_mouse_hook(ready_tx: Sender<Result<u32, String>>) {
    let mut msg = unsafe { std::mem::zeroed::<MSG>() };
    unsafe {
        PeekMessageW(
            &mut msg,
            std::ptr::null_mut(),
            WM_USER,
            WM_USER,
            PM_NOREMOVE,
        );
    }
    let hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), std::ptr::null_mut(), 0) };
    if hook.is_null() {
        let _ = ready_tx.send(Err(
            "low-level mouse hotkey hook could not be installed".into(),
        ));
        return;
    }

    let _ = ready_tx.send(Ok(unsafe { GetCurrentThreadId() }));
    while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    unsafe {
        UnhookWindowsHookEx(hook);
    }
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let message = wparam as u32;
        let keyboard = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        match message {
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                if is_supported_keyboard_hook_vk(keyboard.vkCode) {
                    dispatch_key_down(keyboard.vkCode, (keyboard.flags & LLKHF_ALTDOWN) != 0);
                }
            }
            WM_KEYUP | WM_SYSKEYUP => {
                release_key(keyboard.vkCode);
            }
            _ => {}
        }
    }
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let message = wparam as u32;
        match message {
            WM_MBUTTONDOWN => trigger_mouse_hotkey(VK_MBUTTON_CODE),
            WM_MBUTTONUP => release_mouse_hotkey(VK_MBUTTON_CODE),
            WM_XBUTTONDOWN | WM_XBUTTONUP => {
                let mouse = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
                if let Some(vk_code) = xbutton_vk_code(mouse.mouseData) {
                    if message == WM_XBUTTONDOWN {
                        trigger_mouse_hotkey(vk_code);
                    } else {
                        release_mouse_hotkey(vk_code);
                    }
                }
            }
            _ => {}
        }
    }
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

fn trigger_mouse_hotkey(vk_code: u32) {
    dispatch_key_down(vk_code, false);
}

fn dispatch_key_down(vk_code: u32, alt_flag: bool) {
    let ctrl = key_is_down(VK_CONTROL_CODE);
    let shift = key_is_down(VK_SHIFT_CODE);
    let alt = alt_flag || key_is_down(VK_ALT_CODE);
    if let Some(state) = HOTKEY_HOOK.get() {
        state.on_key_down(vk_code, ctrl, alt, shift);
    }
}

fn release_mouse_hotkey(vk_code: u32) {
    release_key(vk_code);
}

fn release_key(vk_code: u32) {
    if let Some(state) = HOTKEY_HOOK.get() {
        state.on_key_up(vk_code);
    }
}

fn is_supported_keyboard_hook_vk(vk_code: u32) -> bool {
    (VK_F1_CODE..=VK_F24_CODE).contains(&vk_code)
        || (0x30..=0x39).contains(&vk_code)
        || (0x41..=0x5A).contains(&vk_code)
        || matches!(
            vk_code,
            0x08 | 0x09
                | 0x0D
                | 0x20
                | 0x21
                | 0x22
                | 0x23
                | 0x24
                | 0x25
                | 0x26
                | 0x27
                | 0x28
                | 0x2D
                | 0x2E
                | 0xBA
                | 0xBB
                | 0xBC
                | 0xBD
                | 0xBE
                | 0xBF
                | 0xC0
                | 0xDB
                | 0xDC
                | 0xDD
                | 0xDE
        )
}

fn keyboard_key_vk_code(key: &str) -> Option<u32> {
    if key.len() == 1 {
        let c = key.as_bytes()[0];
        if c.is_ascii_uppercase() || c.is_ascii_digit() {
            return Some(c as u32);
        }
    }

    match key {
        "Backspace" => Some(0x08),
        "Tab" => Some(0x09),
        "Enter" => Some(0x0D),
        "Space" => Some(0x20),
        "PageUp" => Some(0x21),
        "PageDown" => Some(0x22),
        "End" => Some(0x23),
        "Home" => Some(0x24),
        "ArrowLeft" => Some(0x25),
        "ArrowUp" => Some(0x26),
        "ArrowRight" => Some(0x27),
        "ArrowDown" => Some(0x28),
        "Insert" => Some(0x2D),
        "Delete" => Some(0x2E),
        "Semicolon" => Some(0xBA),
        "Equal" => Some(0xBB),
        "Comma" => Some(0xBC),
        "Minus" => Some(0xBD),
        "Period" => Some(0xBE),
        "Slash" => Some(0xBF),
        "Backquote" => Some(0xC0),
        "BracketLeft" => Some(0xDB),
        "Backslash" => Some(0xDC),
        "BracketRight" => Some(0xDD),
        "Quote" => Some(0xDE),
        _ => None,
    }
}

fn xbutton_vk_code(mouse_data: u32) -> Option<u32> {
    match ((mouse_data >> 16) & 0xffff) as u16 {
        XBUTTON1 => Some(VK_XBUTTON1_CODE),
        XBUTTON2 => Some(VK_XBUTTON2_CODE),
        _ => None,
    }
}

fn key_is_down(vk: i32) -> bool {
    unsafe { (GetAsyncKeyState(vk) & i16::MIN) != 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn save_only<'a>(save: &'a [&'a str]) -> HookHotkeys<'a> {
        HookHotkeys {
            save,
            ..HookHotkeys::default()
        }
    }

    /// Which action fired; the press stamp rides along but is not comparable.
    fn recv_action(rx: &Receiver<HookTrigger>) -> Result<HookAction, mpsc::TryRecvError> {
        rx.try_recv().map(|trigger| trigger.action)
    }

    #[test]
    fn parses_function_key_hotkeys_for_hook_matching() {
        let hotkey = parse_hook_hotkey("Ctrl+Alt+F9").unwrap();

        assert!(hotkey.matches(VK_F1_CODE + 8, true, true, false));
        assert!(!hotkey.matches(VK_F1_CODE + 8, true, true, true));
        assert!(!hotkey.matches(VK_F1_CODE + 9, true, true, false));
    }

    #[test]
    fn parses_modified_keyboard_hotkeys_for_hook_matching() {
        let letter = parse_hook_hotkey("Ctrl+G").unwrap();
        assert!(letter.matches(0x47, true, false, false));
        assert!(!letter.matches(0x47, false, false, false));

        let literal_f = parse_hook_hotkey("Ctrl+Shift+F").unwrap();
        assert!(literal_f.matches(0x46, true, false, true));
        assert!(!literal_f.matches(VK_F1_CODE, true, false, true));

        let arrow = parse_hook_hotkey("Alt+Shift+ArrowLeft").unwrap();
        assert!(arrow.matches(0x25, false, true, true));
        assert!(!arrow.matches(0x25, false, true, false));

        let slash = parse_hook_hotkey("Ctrl+Slash").unwrap();
        assert!(slash.matches(0xBF, true, false, false));
    }

    #[test]
    fn function_key_range_keeps_distinct_virtual_keys() {
        assert!(parse_hook_hotkey("F1")
            .unwrap()
            .matches(VK_F1_CODE, false, false, false));
        assert!(parse_hook_hotkey("F24")
            .unwrap()
            .matches(VK_F24_CODE, false, false, false));
    }

    #[test]
    fn parses_mouse_button_hotkeys_for_hook_matching() {
        let hotkey = parse_hook_hotkey("Ctrl+Mouse5").unwrap();

        assert!(hotkey.matches(0x06, true, false, false));
        assert!(!hotkey.matches(0x06, false, false, false));
        assert!(!hotkey.matches(0x05, true, false, false));

        let middle = parse_hook_hotkey("Shift+Middle").unwrap();
        assert!(middle.matches(0x04, false, false, true));

        let bare_mouse = parse_hook_hotkey("Mouse4").unwrap();
        assert!(bare_mouse.matches(0x05, false, false, false));
        assert!(!bare_mouse.matches(0x05, true, false, false));
    }

    #[test]
    fn rejects_reserved_f12_through_shared_normalizer() {
        assert!(parse_hook_hotkey("F12").is_err());
    }

    #[test]
    fn hook_does_not_claim_printscreen() {
        assert!(parse_hook_hotkey("PrintScreen").is_err());
        assert!(parse_hook_hotkey("Ctrl+PrintScreen").is_err());
    }

    #[test]
    fn either_configured_hotkey_triggers_a_save() {
        let (trigger_tx, trigger_rx) = mpsc::channel();
        let state = HookState {
            bindings: Mutex::new(
                parse_hook_bindings(save_only(&["Alt+F10", "Ctrl+Mouse5"])).unwrap(),
            ),
            down_keys: Mutex::new(BTreeSet::new()),
            trigger_tx,
            keyboard_hook: None,
            mouse_hook: Mutex::new(None),
            paused: AtomicBool::new(false),
        };

        assert!(state.on_key_down(VK_F1_CODE + 9, false, true, false));
        assert_eq!(recv_action(&trigger_rx), Ok(HookAction::SaveReplay));

        assert!(state.on_key_down(VK_XBUTTON2_CODE, true, false, false));
        assert_eq!(recv_action(&trigger_rx), Ok(HookAction::SaveReplay));

        assert!(!state.on_key_down(VK_F1_CODE + 8, false, true, false));
        assert!(trigger_rx.try_recv().is_err());
    }

    #[test]
    fn save_and_recording_bindings_dispatch_distinct_actions() {
        let (trigger_tx, trigger_rx) = mpsc::channel();
        let state = HookState {
            bindings: Mutex::new(
                parse_hook_bindings(HookHotkeys {
                    save: &["Alt+F10"],
                    recording: &["Ctrl+Mouse5", "Alt+F9"],
                    bookmark: &[],
                    ..Default::default()
                })
                .unwrap(),
            ),
            down_keys: Mutex::new(BTreeSet::new()),
            trigger_tx,
            keyboard_hook: None,
            mouse_hook: Mutex::new(None),
            paused: AtomicBool::new(false),
        };

        assert!(state.on_key_down(VK_F1_CODE + 9, false, true, false));
        assert_eq!(recv_action(&trigger_rx), Ok(HookAction::SaveReplay));
        state.on_key_up(VK_F1_CODE + 9);

        assert!(state.on_key_down(VK_XBUTTON2_CODE, true, false, false));
        assert_eq!(recv_action(&trigger_rx), Ok(HookAction::ToggleRecording));
        state.on_key_up(VK_XBUTTON2_CODE);

        assert!(state.on_key_down(VK_F1_CODE + 8, false, true, false));
        assert_eq!(recv_action(&trigger_rx), Ok(HookAction::ToggleRecording));
    }

    #[test]
    fn bookmark_bindings_dispatch_their_own_action() {
        let (trigger_tx, trigger_rx) = mpsc::channel();
        let state = HookState {
            bindings: Mutex::new(
                parse_hook_bindings(HookHotkeys {
                    save: &["F6"],
                    recording: &["Alt+F9"],
                    bookmark: &["F7", "Ctrl+Mouse5"],
                    ..Default::default()
                })
                .unwrap(),
            ),
            down_keys: Mutex::new(BTreeSet::new()),
            trigger_tx,
            keyboard_hook: None,
            mouse_hook: Mutex::new(None),
            paused: AtomicBool::new(false),
        };

        assert!(state.on_key_down(VK_F1_CODE + 6, false, false, false));
        assert_eq!(recv_action(&trigger_rx), Ok(HookAction::Bookmark));
        state.on_key_up(VK_F1_CODE + 6);

        assert!(state.on_key_down(VK_XBUTTON2_CODE, true, false, false));
        assert_eq!(recv_action(&trigger_rx), Ok(HookAction::Bookmark));
        state.on_key_up(VK_XBUTTON2_CODE);

        assert!(state.on_key_down(VK_F1_CODE + 5, false, false, false));
        assert_eq!(recv_action(&trigger_rx), Ok(HookAction::SaveReplay));
    }

    #[test]
    fn screenshot_bindings_dispatch_their_own_actions() {
        let (trigger_tx, trigger_rx) = mpsc::channel();
        let state = HookState {
            bindings: Mutex::new(
                parse_hook_bindings(HookHotkeys {
                    save: &["F6"],
                    recording: &["Alt+F9"],
                    bookmark: &["F7"],
                    screenshot_region: &["Ctrl+Mouse5"],
                    screenshot_screen: &["Ctrl+Shift+F8"],
                    screenshot_window: &["Ctrl+Alt+F7"],
                })
                .unwrap(),
            ),
            down_keys: Mutex::new(BTreeSet::new()),
            trigger_tx,
            keyboard_hook: None,
            mouse_hook: Mutex::new(None),
            paused: AtomicBool::new(false),
        };

        assert!(state.on_key_down(VK_XBUTTON2_CODE, true, false, false));
        assert_eq!(recv_action(&trigger_rx), Ok(HookAction::ScreenshotRegion));
        state.on_key_up(VK_XBUTTON2_CODE);

        assert!(state.on_key_down(VK_F1_CODE + 7, true, false, true));
        assert_eq!(recv_action(&trigger_rx), Ok(HookAction::ScreenshotScreen));
        state.on_key_up(VK_F1_CODE + 7);

        assert!(state.on_key_down(VK_F1_CODE + 6, true, true, false));
        assert_eq!(recv_action(&trigger_rx), Ok(HookAction::ScreenshotWindow));
    }

    #[test]
    fn a_screenshot_hotkey_cannot_shadow_another_action() {
        assert!(parse_hook_bindings(HookHotkeys {
            save: &["F6"],
            recording: &["Alt+F9"],
            bookmark: &["F7"],
            screenshot_region: &["F6"],
            screenshot_screen: &[],
            screenshot_window: &[],
        })
        .is_err());

        assert!(parse_hook_bindings(HookHotkeys {
            save: &["F6"],
            recording: &["Alt+F9"],
            bookmark: &["F7"],
            screenshot_region: &[],
            screenshot_screen: &["F7"],
            screenshot_window: &[],
        })
        .is_err());

        // Blank entries stay unbound rather than erroring.
        assert!(parse_hook_bindings(HookHotkeys {
            save: &["F6"],
            recording: &["Alt+F9"],
            bookmark: &["F7"],
            screenshot_region: &[""],
            screenshot_screen: &["", ""],
            screenshot_window: &[],
        })
        .is_ok());
    }

    #[test]
    fn paused_hook_does_not_dispatch_the_live_save_bind() {
        let (trigger_tx, trigger_rx) = mpsc::channel();
        let state = HookState {
            bindings: Mutex::new(
                parse_hook_bindings(HookHotkeys {
                    save: &["F6"],
                    recording: &["Alt+F9"],
                    bookmark: &["F7"],
                    ..Default::default()
                })
                .unwrap(),
            ),
            down_keys: Mutex::new(BTreeSet::new()),
            trigger_tx,
            keyboard_hook: None,
            mouse_hook: Mutex::new(None),
            paused: AtomicBool::new(true),
        };

        assert!(!state.on_key_down(VK_F1_CODE + 5, false, false, false));
        assert!(!state.on_key_down(VK_F1_CODE + 6, false, false, false));
        assert!(!state.on_key_down(VK_F1_CODE + 8, false, true, false));
        assert!(trigger_rx.try_recv().is_err());

        state.paused.store(false, Ordering::Release);
        state.on_key_up(VK_F1_CODE + 5);
        assert!(state.on_key_down(VK_F1_CODE + 5, false, false, false));
        assert_eq!(recv_action(&trigger_rx), Ok(HookAction::SaveReplay));
    }

    #[test]
    fn a_bookmark_hotkey_cannot_shadow_another_action() {
        for bookmark in [&["F6"], &["Alt+F9"]] {
            assert!(parse_hook_bindings(HookHotkeys {
                save: &["F6"],
                recording: &["Alt+F9"],
                bookmark,
                ..Default::default()
            })
            .is_err());
        }

        assert!(parse_hook_bindings(HookHotkeys {
            save: &["F6"],
            recording: &["Alt+F9"],
            bookmark: &["F7", "F7"],
            ..Default::default()
        })
        .is_err());

        assert!(parse_hook_bindings(HookHotkeys {
            save: &["F6"],
            recording: &["Alt+F9"],
            bookmark: &["F7", ""],
            ..Default::default()
        })
        .is_ok());
    }

    #[test]
    fn parse_hook_hotkeys_fails_atomically_on_any_invalid_entry() {
        assert!(parse_hook_hotkeys(&["Alt+F10", "F12"]).is_err());
        assert_eq!(parse_hook_hotkeys(&["Alt+F10"]).unwrap().len(), 1);
    }

    #[test]
    fn hook_requirement_tracks_mouse_button_hotkeys() {
        assert!(!parse_hook_hotkey("Alt+F10").unwrap().requires_mouse_hook());
        assert!(parse_hook_hotkey("Ctrl+Mouse5")
            .unwrap()
            .requires_mouse_hook());
        assert!(parse_hook_hotkey("Mouse5").unwrap().requires_mouse_hook());
    }

    #[test]
    fn keyboard_hook_filter_includes_supported_modified_keys() {
        assert!(is_supported_keyboard_hook_vk(0x47));
        assert!(is_supported_keyboard_hook_vk(0x25));
        assert!(is_supported_keyboard_hook_vk(0xBF));
        assert!(is_supported_keyboard_hook_vk(VK_F1_CODE));
        assert!(!is_supported_keyboard_hook_vk(0x1B));
    }

    #[test]
    fn hotkey_lock_contention_waits_instead_of_dropping_trigger() {
        let (trigger_tx, trigger_rx) = mpsc::channel();
        let state = Arc::new(HookState {
            bindings: Mutex::new(parse_hook_bindings(save_only(&["Ctrl+Mouse5"])).unwrap()),
            down_keys: Mutex::new(BTreeSet::new()),
            trigger_tx,
            keyboard_hook: None,
            mouse_hook: Mutex::new(None),
            paused: AtomicBool::new(false),
        });
        let guard = state.bindings.lock().unwrap();
        let worker_state = Arc::clone(&state);
        let worker =
            thread::spawn(move || worker_state.on_key_down(VK_XBUTTON2_CODE, true, false, false));

        thread::sleep(std::time::Duration::from_millis(25));
        assert!(trigger_rx.try_recv().is_err());
        let released_at = Instant::now();
        drop(guard);

        assert!(worker.join().unwrap());
        let trigger = trigger_rx
            .recv_timeout(std::time::Duration::from_millis(250))
            .expect("a delayed trigger still arrives");
        // The stamp is the press, not whenever the hook got its lock back —
        // otherwise a bookmark lands wherever the contention ended.
        assert!(trigger.pressed_at < released_at);
    }

    #[test]
    fn updating_hotkeys_without_an_installed_hook_still_validates_and_succeeds() {
        if HOTKEY_HOOK.get().is_none() {
            assert_eq!(
                set_hotkeys(HookHotkeys {
                    save: &["Alt+F10"],
                    recording: &["Ctrl+R"],
                    bookmark: &["F7"],
                    ..Default::default()
                }),
                Ok(())
            );
            assert!(set_hotkeys(save_only(&["not a hotkey"])).is_err());
        }
    }

    #[test]
    fn keyboard_hook_readiness_reports_success_and_installation_failure() {
        let (success_tx, success_rx) = mpsc::channel();
        success_tx.send(Ok(42)).unwrap();
        assert_eq!(
            wait_for_keyboard_hook_ready(success_rx, std::time::Duration::from_millis(10)).unwrap(),
            KeyboardHookThread { thread_id: 42 }
        );

        let (failure_tx, failure_rx) = mpsc::channel();
        failure_tx.send(Err("hook unavailable".into())).unwrap();
        let error = wait_for_keyboard_hook_ready(failure_rx, std::time::Duration::from_millis(10))
            .unwrap_err();
        assert_eq!(error, "hook unavailable");
    }

    #[test]
    fn keyboard_hook_readiness_reports_disconnect_and_timeout() {
        let (disconnected_tx, disconnected_rx) = mpsc::channel::<Result<u32, String>>();
        drop(disconnected_tx);
        let disconnected =
            wait_for_keyboard_hook_ready(disconnected_rx, std::time::Duration::from_millis(10))
                .unwrap_err();
        assert!(disconnected.contains("disconnected"), "{disconnected}");

        let (_timeout_tx, timeout_rx) = mpsc::channel::<Result<u32, String>>();
        let timeout = wait_for_keyboard_hook_ready(timeout_rx, std::time::Duration::from_millis(1))
            .unwrap_err();
        assert!(timeout.contains("timed out"), "{timeout}");
    }
}
