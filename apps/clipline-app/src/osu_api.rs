//! Thin Tauri compatibility adapter over the shared osu! account and enrichment services.

use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use clipline_desktop::{UiEvent, UiEventSink};
use clipline_games::osu::{
    OsuAccountService, OsuAccountStatus, OsuClientSecret, OsuCredentialPort, OsuSaveRequest,
    OsuSecretError, WindowsOsuCredentialPort,
};
use clipline_games::osu_enrichment::{
    ConfiguredOsuScoreFetcher, JoinedOsuEnrichmentOutcome, JoinedOsuEnrichmentService,
    OsuEnrichmentErrorKind, OsuEnrichmentService, SettingsOsuEnrichmentFence,
};
use clipline_games::osu_http::{OsuHttpClient, OsuHttpConfig, OsuHttpOwner};
use clipline_library::ActiveFileRegistry;
use clipline_settings::{OsuApiSettings, SettingsStore};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, Runtime};

use crate::app::RuntimeState;
use crate::library::StorageSettings;

const OSU_CREDENTIAL_LABEL: &str = "osu! client secret";

#[derive(Debug, Deserialize)]
pub struct SaveOsuApiSettingsRequest {
    pub client_id: String,
    #[serde(default, deserialize_with = "deserialize_optional_secret")]
    pub client_secret: Option<OsuClientSecret>,
    pub user: String,
}

fn deserialize_optional_secret<'de, D>(deserializer: D) -> Result<Option<OsuClientSecret>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let submitted = Option::<String>::deserialize(deserializer)?;
    submitted
        .map(OsuClientSecret::new)
        .transpose()
        .or_else(|error| match error {
            OsuSecretError::Empty => Ok(None),
            other => Err(other),
        })
        .map_err(serde::de::Error::custom)
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OsuApiConnectionStatus {
    pub configured: bool,
    pub secret_present: bool,
    pub client_id: Option<String>,
    pub user: Option<String>,
    pub credential_target: Option<String>,
    pub username: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OsuApiConnectionTestResult {
    pub status: OsuApiConnectionStatus,
    pub score_count: usize,
    pub failed_count: usize,
    pub started_at_count: usize,
    pub ended_at_count: usize,
    pub pagination_ceiling_reached: bool,
}

struct CurrentFence {
    expected: OsuApiSettings,
    fence: Arc<SettingsOsuEnrichmentFence>,
}

struct TauriOsuState {
    accepting: bool,
    terminated: bool,
    active_operations: usize,
    current_fence: Option<CurrentFence>,
}

struct TauriOsuCore {
    state: Mutex<TauriOsuState>,
    changed: Condvar,
}

struct TauriOsuOperation {
    core: Arc<TauriOsuCore>,
}

impl Drop for TauriOsuOperation {
    fn drop(&mut self) {
        let mut state = self
            .core
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active_operations = state.active_operations.saturating_sub(1);
        self.core.changed.notify_all();
    }
}

/// Process-owned Tauri adapter state. The shared coordinator owns its joined
/// worker; this adapter only retains the reusable HTTP pool and current
/// account cancellation fence.
pub struct TauriOsuRuntime {
    http: Option<OsuHttpClient>,
    enrichment: Option<JoinedOsuEnrichmentService>,
    core: Arc<TauriOsuCore>,
    startup_warning: Option<String>,
}

impl TauriOsuRuntime {
    pub fn start() -> Self {
        let http = OsuHttpClient::production();
        let enrichment = JoinedOsuEnrichmentService::start();
        let mut warnings = Vec::new();
        if let Err(error) = &http {
            warnings.push(format!("osu! HTTP unavailable: {error}"));
        }
        if let Err(error) = &enrichment {
            warnings.push(format!("osu! enrichment unavailable: {error}"));
        }
        Self {
            http: http.ok(),
            enrichment: enrichment.ok(),
            core: Arc::new(TauriOsuCore {
                state: Mutex::new(TauriOsuState {
                    accepting: true,
                    terminated: false,
                    active_operations: 0,
                    current_fence: None,
                }),
                changed: Condvar::new(),
            }),
            startup_warning: (!warnings.is_empty()).then(|| warnings.join("; ")),
        }
    }

    #[must_use]
    pub fn startup_warning(&self) -> Option<&str> {
        self.startup_warning.as_deref()
    }

    fn begin_operation(&self) -> Result<TauriOsuOperation, String> {
        let mut state = self
            .core
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.accepting || state.terminated {
            return Err("osu! operations are quiescing".to_string());
        }
        state.active_operations = state
            .active_operations
            .checked_add(1)
            .filter(|active| *active <= 64)
            .ok_or_else(|| "too many osu! operations are active".to_string())?;
        Ok(TauriOsuOperation {
            core: Arc::clone(&self.core),
        })
    }

    fn http(&self) -> Result<&OsuHttpClient, String> {
        self.http
            .as_ref()
            .ok_or_else(|| "osu! HTTP service is unavailable".to_string())
    }

    fn fence_for(
        &self,
        store: SettingsStore,
        expected: OsuApiSettings,
    ) -> Result<Arc<SettingsOsuEnrichmentFence>, String> {
        let mut state = self
            .core
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.accepting || state.terminated {
            return Err("osu! operations are quiescing".to_string());
        }
        if let Some(current) = state.current_fence.as_ref() {
            if same_account_owner(&current.expected, &expected) {
                return Ok(Arc::clone(&current.fence));
            }
            current.fence.cancel();
        }
        let fence = Arc::new(SettingsOsuEnrichmentFence::new(store, expected.clone()));
        state.current_fence = Some(CurrentFence {
            expected,
            fence: Arc::clone(&fence),
        });
        Ok(fence)
    }

    fn with_account_change<T>(
        &self,
        change: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let mut state = self
            .core
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.accepting || state.terminated {
            return Err("osu! operations are quiescing".to_string());
        }
        if let Some(current) = state.current_fence.take() {
            current.fence.cancel();
        }
        // Keep new old-owner work from registering between cancellation and
        // the durable account mutation.
        change()
    }

    fn with_account_read<T>(&self, read: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
        let state = self
            .core
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.accepting || state.terminated {
            return Err("osu! operations are quiescing".to_string());
        }
        // Account changes take the same mutex through `with_account_change`,
        // so the durable status and compatibility mirror are one coherent
        // snapshot from the adapter's point of view.
        read()
    }

    fn cancel_matching(&self, expected: &OsuApiSettings) {
        let mut state = self
            .core
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .current_fence
            .as_ref()
            .is_some_and(|current| same_account_owner(&current.expected, expected))
        {
            if let Some(current) = state.current_fence.take() {
                current.fence.cancel();
            }
        }
    }

    pub fn quiesce_and_wait(&self, timeout: Duration) -> Result<(), String> {
        let started = Instant::now();
        let mut state = self
            .core
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.terminated {
            return Ok(());
        }
        state.accepting = false;
        if let Some(current) = state.current_fence.take() {
            current.fence.cancel();
        }
        while state.active_operations != 0 {
            let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
                state.accepting = true;
                return Err("timed out quiescing osu! operations".to_string());
            };
            let (next, wait) = self
                .core
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next;
            if wait.timed_out() && state.active_operations != 0 {
                state.accepting = true;
                return Err("timed out quiescing osu! operations".to_string());
            }
        }
        Ok(())
    }

    pub fn resume(&self) -> Result<(), String> {
        let mut state = self
            .core
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.terminated {
            return Err("osu! runtime is shut down".to_string());
        }
        state.accepting = true;
        Ok(())
    }

    pub fn commit_shutdown(&self) -> Result<(), String> {
        let mut state = self
            .core
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.terminated {
            return Ok(());
        }
        if state.active_operations != 0 {
            return Err("osu! operations are still active at shutdown".to_string());
        }
        state.accepting = false;
        state.terminated = true;
        if let Some(current) = state.current_fence.take() {
            current.fence.cancel();
        }
        drop(state);
        self.enrichment.as_ref().map_or(Ok(()), |enrichment| {
            enrichment.shutdown().map_err(|error| error.to_string())
        })
    }

    pub fn shutdown(&self) -> Result<(), String> {
        self.quiesce_and_wait(Duration::from_secs(15))?;
        self.commit_shutdown()
    }

    fn submit_enrichment(
        &self,
        media_root: &std::path::Path,
        now_unix: u64,
        pass: Arc<dyn clipline_games::osu_enrichment::OsuEnrichmentPass>,
        fence: Arc<dyn clipline_games::osu_enrichment::OsuEnrichmentFence>,
    ) -> Result<clipline_games::osu_enrichment::JoinedOsuEnrichmentHandle, String> {
        self.enrichment
            .as_ref()
            .ok_or_else(|| "osu! enrichment service is unavailable".to_string())?
            .submit(media_root, now_unix, pass, fence)
            .map_err(|error| error.to_string())
    }
}

impl Drop for TauriOsuRuntime {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn same_account_owner(left: &OsuApiSettings, right: &OsuApiSettings) -> bool {
    left.account_generation == right.account_generation
        && left.client_id == right.client_id
        && left.user == right.user
        && left.credential_target == right.credential_target
}

fn account_service(
    store: SettingsStore,
) -> OsuAccountService<WindowsOsuCredentialPort, SettingsStore> {
    OsuAccountService::new(WindowsOsuCredentialPort::new(OSU_CREDENTIAL_LABEL), store)
}

fn compatibility_status(
    status: OsuAccountStatus,
    profile: &OsuApiSettings,
) -> OsuApiConnectionStatus {
    OsuApiConnectionStatus {
        configured: status.configured,
        secret_present: status.secret_present,
        client_id: status.client_id,
        user: status.user,
        credential_target: profile.credential_target.clone(),
        username: status.username,
    }
}

fn coherent_status(
    state: &RuntimeState,
    service: &OsuAccountService<WindowsOsuCredentialPort, SettingsStore>,
) -> Result<(OsuAccountStatus, OsuApiSettings), String> {
    for _ in 0..3 {
        let status = service.status().map_err(|error| error.to_string())?;
        let profile = state.refresh_osu_profile()?;
        if status.account_generation == profile.account_generation
            && status.client_id == profile.client_id
            && status.user == profile.user
        {
            return Ok((status, profile));
        }
    }
    Err("osu! account changed while preparing compatibility status".to_string())
}

fn exact_profile_for_status(
    state: &RuntimeState,
    status: &OsuAccountStatus,
) -> Result<OsuApiSettings, String> {
    let profile = state.refresh_osu_profile()?;
    if status.account_generation == profile.account_generation
        && status.client_id == profile.client_id
        && status.user == profile.user
    {
        Ok(profile)
    } else {
        Err("osu! account changed before the compatibility response was published".to_string())
    }
}

#[tauri::command]
pub fn osu_api_status(
    state: tauri::State<'_, RuntimeState>,
    runtime: tauri::State<'_, TauriOsuRuntime>,
) -> Result<OsuApiConnectionStatus, String> {
    let _operation = runtime.begin_operation()?;
    let store = state.osu_settings_store()?;
    let service = account_service(store);
    let (status, profile) = runtime.with_account_read(|| coherent_status(&state, &service))?;
    Ok(compatibility_status(status, &profile))
}

#[tauri::command]
pub fn save_osu_api_settings(
    state: tauri::State<'_, RuntimeState>,
    runtime: tauri::State<'_, TauriOsuRuntime>,
    request: SaveOsuApiSettingsRequest,
) -> Result<OsuApiConnectionStatus, String> {
    let _operation = runtime.begin_operation()?;
    let store = state.osu_settings_store()?;
    let service = account_service(store);
    let status = runtime.with_account_change(|| {
        service
            .save(OsuSaveRequest {
                client_id: request.client_id,
                user: request.user,
                client_secret: request.client_secret,
            })
            .map_err(|error| error.to_string())
    })?;
    let profile = runtime.with_account_read(|| exact_profile_for_status(&state, &status))?;
    Ok(compatibility_status(status, &profile))
}

#[tauri::command]
pub async fn test_osu_api_connection<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, RuntimeState>,
    storage: tauri::State<'_, StorageSettings>,
    runtime: tauri::State<'_, TauriOsuRuntime>,
) -> Result<OsuApiConnectionTestResult, String> {
    let _operation = runtime.begin_operation()?;
    let store = state.osu_settings_store()?;
    let expected = store
        .current_osu_profile()
        .map_err(|error| error.to_string())?;
    let fence = runtime.fence_for(store.clone(), expected.clone())?;
    let service = account_service(store);
    let result = service
        .test(runtime.http()?, fence.as_ref())
        .await
        .map_err(|error| error.to_string())?;
    runtime.cancel_matching(&expected);
    let profile = runtime.with_account_read(|| exact_profile_for_status(&state, &result.status))?;
    let response = OsuApiConnectionTestResult {
        status: compatibility_status(result.status, &profile),
        score_count: result.score_count,
        failed_count: result.failed_count,
        started_at_count: result.started_at_count,
        ended_at_count: result.ended_at_count,
        pagination_ceiling_reached: result.pagination_ceiling_reached,
    };
    if let Err(error) = retry_pending_enrichment(&app, storage.media_dir()).await {
        tracing::warn!(event = "osu_enrichment_retry_failed", error = %error);
    }
    Ok(response)
}

#[tauri::command]
pub fn open_osu_api_setup_guide() -> Result<(), String> {
    let path = crate::settings::persistence::config_base().join("osu-api-setup-guide.html");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create osu! guide dir: {error}"))?;
    }
    std::fs::write(&path, osu_setup_guide_html())
        .map_err(|error| format!("write osu! setup guide: {error}"))?;
    open_path(&path, "osu! API setup guide")
}

pub async fn retry_pending_enrichment<R: Runtime>(
    app: &AppHandle<R>,
    media_root: PathBuf,
) -> Result<(), String> {
    let runtime = app.state::<TauriOsuRuntime>();
    let _operation = runtime.begin_operation()?;
    let state = app.state::<RuntimeState>();
    let store = state.osu_settings_store()?;
    let expected = store
        .current_osu_profile()
        .map_err(|error| error.to_string())?;
    let (Some(client_id), Some(user), Some(target)) = (
        expected.client_id.clone(),
        expected.user.clone(),
        expected.credential_target.as_deref(),
    ) else {
        return Ok(());
    };
    let credentials = WindowsOsuCredentialPort::new(OSU_CREDENTIAL_LABEL);
    let Some(secret) = credentials
        .read(target)
        .map_err(|_| "read osu! client secret failed".to_string())?
    else {
        return Ok(());
    };
    let config = OsuHttpConfig::new(
        OsuHttpOwner::new(expected.account_generation),
        client_id,
        user,
        secret,
    )
    .map_err(|error| error.to_string())?;
    let fence = runtime.fence_for(store, expected)?;
    let pass = Arc::new(OsuEnrichmentService::new(
        ConfiguredOsuScoreFetcher::new(runtime.http()?.clone(), config),
        Arc::new(app.state::<ActiveFileRegistry>().inner().clone()),
    ));
    let handle = runtime.submit_enrichment(&media_root, crate::util::unix_now(), pass, fence)?;
    let outcome = tauri::async_runtime::spawn_blocking(move || handle.recv())
        .await
        .map_err(|error| format!("join osu! enrichment result wait: {error}"))?
        .map_err(|_| "osu! enrichment result channel disconnected".to_string())?;
    let updated = match outcome {
        JoinedOsuEnrichmentOutcome::Completed(summary) => summary.updated > 0,
        JoinedOsuEnrichmentOutcome::Failed(error)
            if matches!(
                error.kind(),
                OsuEnrichmentErrorKind::AccountChanged | OsuEnrichmentErrorKind::Canceled
            ) =>
        {
            false
        }
        JoinedOsuEnrichmentOutcome::Failed(error) => return Err(error.to_string()),
        JoinedOsuEnrichmentOutcome::Superseded => false,
        JoinedOsuEnrichmentOutcome::Panicked => {
            return Err("osu! enrichment worker panicked".to_string())
        }
        JoinedOsuEnrichmentOutcome::ShutDown => {
            return Err("osu! enrichment coordinator shut down".to_string())
        }
    };
    if updated {
        let generation = app
            .state::<crate::desktop::ProducerGenerations>()
            .next_enrichment()?;
        let _ = app
            .state::<crate::desktop::tauri_sink::TauriUiEventSink>()
            .try_publish(UiEvent::EnrichmentUpdated { generation });
    }
    Ok(())
}

fn open_path(path: &std::path::Path, context: &str) -> Result<(), String> {
    clipline_shell::windows::shell_execute::open_path(path, context)
        .map_err(|error| error.to_string())
}

fn osu_setup_guide_html() -> &'static str {
    r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Clipline osu! API setup</title>
  <style>
    :root { color-scheme: dark; font-family: Inter, Segoe UI, sans-serif; background: #111317; color: #f5f7fb; }
    body { margin: 0; padding: 32px; line-height: 1.5; }
    main { max-width: 780px; margin: 0 auto; }
    h1 { margin: 0 0 8px; font-size: 28px; }
    h2 { margin-top: 28px; font-size: 18px; }
    a { color: #ff8ac6; }
    code { padding: 2px 5px; border-radius: 4px; background: #20242c; }
    li { margin: 8px 0; }
    .note { padding: 12px 14px; border: 1px solid #343946; border-radius: 8px; background: #181b22; }
  </style>
</head>
<body>
<main>
  <h1>Clipline osu! API setup</h1>
  <p class="note">Clipline uses an osu! OAuth app with the client credentials grant. Your client secret is stored locally in Windows Credential Manager and is never written to settings.json.</p>
  <h2>Create the osu! OAuth app</h2>
  <ol>
    <li>Open <a href="https://osu.ppy.sh/home/account/edit#oauth" target="_blank" rel="noreferrer">osu! account OAuth settings</a>.</li>
    <li>Create a new OAuth application.</li>
    <li>Name it <code>Clipline</code> or another name you recognize.</li>
    <li>For Application Callback URL, enter <code>http://127.0.0.1</code>. Clipline does not use the callback for this direct API mode, but osu! requires a value.</li>
    <li>Copy the Client ID and Client Secret into Clipline.</li>
    <li>Enter your osu! user id or username, then click <strong>Test osu! API connection</strong>.</li>
  </ol>
  <h2>What Clipline reads</h2>
  <p>Clipline requests only the public scope and fetches recent osu!standard scores, including failed submitted plays when osu! returns them.</p>
</main>
</body>
</html>
"#
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_games::osu_http::OsuRequestFence;
    use clipline_settings::{SettingsProfile, SettingsStore};
    use clipline_test_utils::TestDir;

    #[test]
    fn request_deserialization_immediately_moves_the_secret_into_a_redacted_owner() {
        let request: SaveOsuApiSettingsRequest = serde_json::from_value(serde_json::json!({
            "client_id": "61835",
            "client_secret": "super-secret",
            "user": "Dain"
        }))
        .unwrap();

        let rendered = format!("{request:?}");
        assert!(rendered.contains("[REDACTED]"));
        assert!(!rendered.contains("super-secret"));

        for secret in [
            None,
            Some(serde_json::Value::Null),
            Some(serde_json::json!("   ")),
        ] {
            let mut value = serde_json::json!({"client_id": "61835", "user": "Dain"});
            if let Some(secret) = secret {
                value["client_secret"] = secret;
            }
            let request: SaveOsuApiSettingsRequest = serde_json::from_value(value).unwrap();
            assert!(request.client_secret.is_none());
        }
    }

    #[test]
    fn compatibility_status_keeps_the_existing_snake_case_json_shape() {
        let generation = clipline_settings::OsuAccountGeneration::default();
        let status = compatibility_status(
            OsuAccountStatus {
                account_generation: generation,
                configured: true,
                secret_present: true,
                client_id: Some("61835".into()),
                user: Some("3426414".into()),
                username: Some("Dain".into()),
            },
            &OsuApiSettings {
                credential_target: Some("Clipline osu!:target".into()),
                ..OsuApiSettings::default()
            },
        );

        assert_eq!(
            serde_json::to_value(status).unwrap(),
            serde_json::json!({
                "configured": true,
                "secret_present": true,
                "client_id": "61835",
                "user": "3426414",
                "credential_target": "Clipline osu!:target",
                "username": "Dain"
            })
        );
    }

    #[test]
    fn compatibility_projection_rejects_a_profile_from_another_account_generation() {
        let dir = TestDir::new("clipline-osu-api", "tauri-status-owner");
        let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
        let initial = store.snapshot().unwrap();
        let state = RuntimeState::with_store(initial.document.clone(), None, store.clone());
        let stale_status = OsuAccountStatus {
            account_generation: initial.document.osu.account_generation,
            configured: false,
            secret_present: false,
            client_id: None,
            user: None,
            username: None,
        };
        let mut replacement = initial.document.osu;
        replacement.account_generation = replacement.account_generation.checked_next().unwrap();
        store
            .transact(clipline_settings::SettingsTransaction {
                expected_revision: initial.revision,
                expected_account_generation: initial.account_generation,
                change: clipline_settings::SettingsChange::ReplaceOsuProfile(replacement),
            })
            .unwrap();

        let error = exact_profile_for_status(&state, &stale_status).unwrap_err();

        assert!(error.contains("account changed"));
        assert_ne!(
            state.settings().osu.account_generation,
            stale_status.account_generation
        );
    }

    #[test]
    fn runtime_reuses_one_account_fence_and_cancels_that_exact_owner() {
        let dir = TestDir::new("clipline-osu-api", "tauri-fence");
        let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
        let expected = store.current_osu_profile().unwrap();
        let runtime = TauriOsuRuntime::start();
        let first = runtime.fence_for(store.clone(), expected.clone()).unwrap();
        let second = runtime.fence_for(store, expected.clone()).unwrap();
        assert!(Arc::ptr_eq(&first, &second));

        runtime.cancel_matching(&expected);

        assert!(!first.is_current(first.owner()));
        runtime.shutdown().unwrap();
    }

    #[test]
    fn quiescence_waits_for_operations_then_resumes_or_seals_exactly_once() {
        let runtime = Arc::new(TauriOsuRuntime::start());
        let operation = runtime.begin_operation().unwrap();
        let waiter = Arc::clone(&runtime);
        let (sent, received) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            sent.send(waiter.quiesce_and_wait(Duration::from_secs(2)))
                .unwrap();
        });

        assert!(received.recv_timeout(Duration::from_millis(50)).is_err());
        drop(operation);
        received
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        worker.join().unwrap();
        assert!(runtime.begin_operation().is_err());

        runtime.resume().unwrap();
        drop(runtime.begin_operation().unwrap());
        runtime.quiesce_and_wait(Duration::from_secs(1)).unwrap();
        runtime.commit_shutdown().unwrap();
        runtime.commit_shutdown().unwrap();
        assert!(runtime.begin_operation().is_err());
        assert!(runtime.resume().is_err());
    }

    #[test]
    fn setup_guide_keeps_the_trusted_oauth_destination_and_local_secret_copy() {
        let guide = osu_setup_guide_html();
        assert!(guide.contains("https://osu.ppy.sh/home/account/edit#oauth"));
        assert!(guide.contains("Windows Credential Manager"));
        assert!(guide.contains("http://127.0.0.1"));
    }
}
