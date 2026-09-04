use std::time::Duration;

use tauri::{
    AppHandle, Emitter, Manager, Runtime,
};
use tauri_plugin_updater::UpdaterExt;


use crate::service::Cmd;
use crate::updates::UpdateChannel;
use super::*;

#[derive(serde::Serialize)]
pub(crate) struct UpdateCheckResult {
    pub(crate) channel: UpdateChannel,
    pub(crate) channel_label: &'static str,
    pub(crate) current_version: String,
    pub(crate) available: bool,
    pub(crate) version: Option<String>,
    pub(crate) date: Option<String>,
    pub(crate) endpoint: &'static str,
    pub(crate) status: Option<String>,
}

pub(crate) async fn check_update_for_channel<R: Runtime>(
    app: &AppHandle<R>,
    channel: UpdateChannel,
) -> Result<(Option<tauri_plugin_updater::Update>, Option<String>), String> {
    if !channel.enabled() {
        return Err(format!("{} updates are not available yet", channel.label()));
    }

    let endpoint = channel
        .endpoint(is_standalone_install(app))
        .parse()
        .map_err(|e| format!("parse update endpoint: {e}"))?;
    let updater = app
        .updater_builder()
        .timeout(Duration::from_secs(20))
        .endpoints(vec![endpoint])
        .map_err(|e| e.to_string())?
        .build()
        .map_err(|e| e.to_string())?;

    match updater.check().await {
        Ok(update) => Ok((update, None)),
        Err(tauri_plugin_updater::Error::ReleaseNotFound) => {
            Ok((None, Some(missing_release_metadata_message(channel))))
        }
        Err(e) => Err(e.to_string()),
    }
}

/// How long after launch the first background update check runs. Long enough
/// that it does not compete with recorder startup, short enough that someone
/// who just opened Clipline learns about a waiting build.
pub(crate) const UPDATE_POLL_FIRST_DELAY: Duration = Duration::from_secs(30);

/// Gap between background update checks. The endpoint is a GitHub release
/// asset served from their CDN, not the rate-limited REST API, so ~144 checks
/// a day of a roughly 1 KB JSON costs nothing worth saving.
pub(crate) const UPDATE_POLL_INTERVAL: Duration = Duration::from_secs(600);

/// Whether a completed check should end the poll. Finding an update is
/// terminal: the rail button stays until the user acts on it, and someone who
/// declined the install should not be asked again every ten minutes.
pub(crate) fn update_poll_is_done(result: &UpdateCheckResult) -> bool {
    result.available
}

/// Poll for a newer build in the background and announce the first one found.
/// The webview cannot own this: the window closes to tray while the recorder
/// keeps running, and a closed window would stop checking.
pub(crate) fn spawn_update_poller<R: Runtime>(app: AppHandle<R>) {
    tauri::async_runtime::spawn(async move {
        let mut delay = UPDATE_POLL_FIRST_DELAY;
        loop {
            tokio::time::sleep(delay).await;
            delay = UPDATE_POLL_INTERVAL;

            let channel = app.state::<RuntimeState>().settings().update_channel;
            // A channel with nothing published behind it is not an error worth
            // reporting every ten minutes.
            if !channel.enabled() {
                continue;
            }

            match update_check_result(&app, channel).await {
                Ok(result) => {
                    if update_poll_is_done(&result) {
                        tracing::info!(
                            event = "update_available",
                            version = result.version.as_deref().unwrap_or("unknown")
                        );
                        let _ = app.emit("update-available", &result);
                        return;
                    }
                }
                // Offline, DNS down, GitHub blipping — all expected. Keep the
                // same interval rather than retrying tighter.
                Err(error) => {
                    tracing::debug!(event = "update_check_failed", error = %error);
                }
            }
        }
    });
}

pub(crate) fn missing_release_metadata_message(channel: UpdateChannel) -> String {
    format!(
        "No {} release metadata is published yet. Publish a {} release first.",
        channel.label(),
        channel.label()
    )
}

/// One update check, shaped for both the Settings button and the background
/// poll so the two can never disagree about what "available" means.
pub(crate) async fn update_check_result<R: Runtime>(
    app: &AppHandle<R>,
    channel: UpdateChannel,
) -> Result<UpdateCheckResult, String> {
    let current_version = app.package_info().version.to_string();
    let (update, status) = check_update_for_channel(app, channel).await?;

    Ok(UpdateCheckResult {
        channel,
        channel_label: channel.label(),
        current_version,
        available: update.is_some(),
        version: update.as_ref().map(|update| update.version.clone()),
        date: update
            .as_ref()
            .and_then(|update| update.date.map(|date| date.to_string())),
        endpoint: channel.endpoint(is_standalone_install(app)),
        status,
    })
}

/// A manual Check must honor the channel shown in Settings, which can differ
/// from the saved value until Save is pressed; launch and background checks
/// have no selection in hand and always use the saved channel.
pub(crate) fn resolve_update_channel(selected: Option<UpdateChannel>, saved: UpdateChannel) -> UpdateChannel {
    selected.unwrap_or(saved)
}

#[tauri::command]
pub(crate) async fn check_for_updates<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, RuntimeState>,
    channel: Option<UpdateChannel>,
) -> Result<UpdateCheckResult, String> {
    let channel = resolve_update_channel(channel, state.settings().update_channel);
    update_check_result(&app, channel).await
}

#[tauri::command]
pub(crate) async fn install_update<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, RuntimeState>,
    channel: Option<UpdateChannel>,
) -> Result<(), String> {
    let channel = resolve_update_channel(channel, state.settings().update_channel);
    let (update, status) = check_update_for_channel(&app, channel).await?;
    let Some(update) = update else {
        return Err(status.unwrap_or_else(|| "no update is available".into()));
    };

    app.state::<MicTestState>().stop();
    state.send(Cmd::Stop { announce: false });
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_and_nightly_channels_can_check_updates() {
        assert!(UpdateChannel::Stable.enabled());
        assert!(UpdateChannel::Nightly.enabled());
    }

    #[test]
    fn missing_release_metadata_message_names_channel_workflow() {
        assert_eq!(
            missing_release_metadata_message(UpdateChannel::Nightly),
            "No Nightly release metadata is published yet. Publish a Nightly release first."
        );
        assert_eq!(
            missing_release_metadata_message(UpdateChannel::Stable),
            "No Stable release metadata is published yet. Publish a Stable release first."
        );
    }

    #[test]
    fn manual_channel_selection_overrides_saved_channel() {
        assert_eq!(
            resolve_update_channel(Some(UpdateChannel::Stable), UpdateChannel::Nightly),
            UpdateChannel::Stable
        );
        assert_eq!(
            resolve_update_channel(None, UpdateChannel::Stable),
            UpdateChannel::Stable
        );
    }
}
