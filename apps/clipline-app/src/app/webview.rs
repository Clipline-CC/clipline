use std::sync::atomic::Ordering;
use std::sync::Mutex;

use tauri::{
    AppHandle, Emitter, Manager, Runtime, WebviewWindow,
};


use super::*;

pub(crate) fn result_debug<T, E>(result: Result<T, E>) -> String
where
    T: std::fmt::Debug,
    E: std::fmt::Display,
{
    match result {
        Ok(value) => format!("ok({value:?})"),
        Err(e) => format!("err({e})"),
    }
}

pub(crate) fn webview_labels<R: Runtime>(app: &AppHandle<R>) -> String {
    let mut labels = app.webview_windows().into_keys().collect::<Vec<_>>();
    labels.sort();
    format!("[{}]", labels.join(","))
}

pub(crate) fn is_app_window_label(label: &str) -> bool {
    label == MAIN_WINDOW_LABEL
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WebviewRepairNoticeReason {
    GetterFailedToReceiveMessage,
    FrontendReadyTimeout,
    OtherGetterError,
}

pub(crate) fn classify_webview_getter_error(error: &tauri::Error) -> WebviewRepairNoticeReason {
    match error {
        tauri::Error::Runtime(tauri_runtime::Error::FailedToReceiveMessage) => {
            WebviewRepairNoticeReason::GetterFailedToReceiveMessage
        }
        _ => WebviewRepairNoticeReason::OtherGetterError,
    }
}

pub(crate) fn should_show_webview_repair_notice(
    reason: WebviewRepairNoticeReason,
    already_shown: bool,
) -> bool {
    !already_shown
        && matches!(
            reason,
            WebviewRepairNoticeReason::GetterFailedToReceiveMessage
                | WebviewRepairNoticeReason::FrontendReadyTimeout
        )
}

pub(crate) fn show_webview_repair_notice_once(reason: WebviewRepairNoticeReason) {
    if !should_show_webview_repair_notice(
        reason,
        WEBVIEW_REPAIR_NOTICE_SHOWN.load(Ordering::Relaxed),
    ) {
        return;
    }
    if WEBVIEW_REPAIR_NOTICE_SHOWN
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return;
    }

    log_diagnostic(format!("webview2 repair notice shown reason={reason:?}"));
    let _ = std::thread::Builder::new()
        .name("clipline-webview2-repair-notice".into())
        .spawn(move || {
            let _ = rfd::MessageDialog::new()
                .set_title("Clipline needs Microsoft WebView2")
                .set_description(
                    "Clipline is running, but the Windows WebView2 runtime did not start. \
Install or repair Microsoft Edge WebView2 Runtime, then reopen Clipline.\n\n\
You can get it from Microsoft: https://developer.microsoft.com/microsoft-edge/webview2/",
                )
                .set_buttons(rfd::MessageButtons::Ok)
                .show();
        });
}

pub(crate) fn probe_webview_after_reveal<R: Runtime>(window: &WebviewWindow<R>, context: &str) {
    match window.is_visible() {
        Ok(visible) => log_diagnostic(format!("{context} health probe is_visible=ok({visible})")),
        Err(e) => {
            let reason = classify_webview_getter_error(&e);
            log_diagnostic(format!(
                "{context} health probe is_visible=err({e}) reason={reason:?}"
            ));
            show_webview_repair_notice_once(reason);
        }
    }
}

pub(crate) fn arm_frontend_ready_watchdog<R: Runtime>(app: &AppHandle<R>, generation: u64) {
    let readiness = app.state::<FrontendReadinessState>();
    if !readiness.try_arm_watchdog(generation) {
        return;
    }

    log_diagnostic(format!(
        "webview readiness watchdog armed generation={generation}"
    ));
    let app = app.clone();
    let _ = std::thread::Builder::new()
        .name("clipline-webview-readiness-watchdog".into())
        .spawn(move || {
            std::thread::sleep(WEBVIEW_READY_TIMEOUT);
            let readiness = app.state::<FrontendReadinessState>();
            if watchdog_should_fire(
                generation,
                readiness.generation(),
                readiness.ready_generation(),
            ) {
                log_diagnostic(format!(
                    "webview readiness watchdog expired before frontend_ready generation={generation}"
                ));
                show_webview_repair_notice_once(WebviewRepairNoticeReason::FrontendReadyTimeout);
            } else {
                log_diagnostic(format!(
                    "webview readiness watchdog settled generation={generation} current={} ready={}",
                    readiness.generation(),
                    readiness.ready_generation()
                ));
            }
        });
}

pub(crate) const WEBVIEW2_RUNTIME_CLIENT_GUID: &str = "{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}";

pub(crate) fn webview2_runtime_registry_keys() -> [String; 3] {
    [
        format!(
            r"HKLM\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{WEBVIEW2_RUNTIME_CLIENT_GUID}"
        ),
        format!(r"HKLM\SOFTWARE\Microsoft\EdgeUpdate\Clients\{WEBVIEW2_RUNTIME_CLIENT_GUID}"),
        format!(r"HKCU\Software\Microsoft\EdgeUpdate\Clients\{WEBVIEW2_RUNTIME_CLIENT_GUID}"),
    ]
}

pub(crate) fn parse_reg_pv_output(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let name = fields.next()?;
        let kind = fields.next()?;
        if !name.eq_ignore_ascii_case("pv") || !kind.eq_ignore_ascii_case("REG_SZ") {
            return None;
        }
        let value = fields.collect::<Vec<_>>().join(" ");
        (!value.is_empty()).then_some(value)
    })
}

pub(crate) fn query_registry_pv(key: &str) -> Option<String> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let output = std::process::Command::new("reg.exe")
        .args(["query", key, "/v", "pv"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_reg_pv_output(&String::from_utf8_lossy(&output.stdout))
}

pub(crate) fn webview2_runtime_diagnostic() -> String {
    let entries = webview2_runtime_registry_keys()
        .into_iter()
        .map(|key| {
            let version = query_registry_pv(&key).unwrap_or_else(|| "missing".to_string());
            format!("{key}={version}")
        })
        .collect::<Vec<_>>();
    format!("webview2_runtime_versions {}", entries.join("; "))
}

#[tauri::command]
pub(crate) async fn memory_status(
    sampler: tauri::State<'_, crate::memory::MemorySampler>,
) -> Result<crate::memory::MemoryStatus, String> {
    sampler.sample().await
}

#[tauri::command]
pub(crate) fn frontend_ready<R: Runtime>(
    app: AppHandle<R>,
    runtime: tauri::State<RuntimeState>,
    startup_warnings: tauri::State<StartupWarnings>,
    window_lifecycle: tauri::State<WindowLifecycleState>,
    readiness: tauri::State<FrontendReadinessState>,
) -> FrontendReadyResponse {
    let generation = readiness.mark_ready();
    log_diagnostic(format!(
        "frontend_ready received generation={} webviews={}",
        generation
            .map(|value| value.to_string())
            .unwrap_or_else(|| "none".into()),
        webview_labels(&app)
    ));
    if let Some(status) = runtime.durable_recorder_status_for_replay() {
        let _ = app.emit("status", status);
    }
    if let Some(game) = runtime.current_game_detection_for_replay() {
        let _ = app.emit("game-detection", game);
    }
    if let Some(event) = runtime.durable_quota_event_for_replay() {
        let _ = app.emit("storage-quota-full", event);
    }
    resume_hotkeys_after_ui_gone(&app);
    FrontendReadyResponse {
        warnings: startup_warnings.snapshot(),
        window_lifecycle: window_lifecycle.snapshot(),
    }
}

#[derive(Default)]
pub(crate) struct StartupWarnings(pub(crate) Mutex<Vec<String>>);

impl StartupWarnings {
    pub(crate) fn new(warnings: Vec<String>) -> Self {
        Self(Mutex::new(warnings))
    }

    pub(crate) fn snapshot(&self) -> Vec<String> {
        match self.0.lock() {
            Ok(warnings) => warnings.clone(),
            Err(error) => vec![format!(
                "startup diagnostics could not be read because their lock was poisoned: {error}"
            )],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_warnings_remain_durable_across_frontend_ready_replays() {
        let warnings = StartupWarnings::new(vec!["settings recovered".into()]);

        assert_eq!(warnings.snapshot(), vec!["settings recovered"]);
        assert_eq!(
            warnings.snapshot(),
            vec!["settings recovered"],
            "recreated UIs must see the same durable startup warnings"
        );
    }

    #[test]
    fn webview_repair_notice_is_only_needed_for_dead_webview_signals() {
        assert!(should_show_webview_repair_notice(
            WebviewRepairNoticeReason::GetterFailedToReceiveMessage,
            false,
        ));
        assert!(should_show_webview_repair_notice(
            WebviewRepairNoticeReason::FrontendReadyTimeout,
            false,
        ));
        assert!(!should_show_webview_repair_notice(
            WebviewRepairNoticeReason::OtherGetterError,
            false,
        ));
        assert!(!should_show_webview_repair_notice(
            WebviewRepairNoticeReason::GetterFailedToReceiveMessage,
            true,
        ));
    }

    #[test]
    fn classifies_tauri_runtime_receive_failure_as_dead_webview() {
        let err = tauri::Error::Runtime(tauri_runtime::Error::FailedToReceiveMessage);

        assert_eq!(
            classify_webview_getter_error(&err),
            WebviewRepairNoticeReason::GetterFailedToReceiveMessage
        );
    }

    #[test]
    fn parses_webview2_runtime_version_from_reg_output() {
        let output = r#"
HKEY_CURRENT_USER\Software\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}
    pv    REG_SZ    120.0.2210.55
"#;

        assert_eq!(
            parse_reg_pv_output(output).as_deref(),
            Some("120.0.2210.55")
        );
    }
}
