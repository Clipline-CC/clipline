//! Bounded native Settings discovery adapter.

use std::collections::HashMap;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use clipline_desktop::{UiEvent, UiEventSink};
use clipline_recorder::probe::SettingsProbeCatalog;
use clipline_settings::{
    ProbeExecutionContext, ProbeExecutor, ProbeKind, ProbeOutcome, ProbeResultReceiver,
    ProbeSessionFence, ProbeSessionOwner, ProbeSubmitOutcome, ProbeToken, SettingsTab,
};

use crate::desktop::tauri_sink::TauriUiEventSink;
use crate::settings::{quota_bytes_from_gb, AppSettings, CustomGameSettings};

const RESULT_POLL: Duration = Duration::from_millis(100);

// Session lifecycle/catalog access is consumed by the native Settings shell
// in Task 11. Task 5 installs and shutdown-tests the executor without teaching
// the compatibility WebView about native session tokens.
#[allow(dead_code)]
pub struct SettingsProbeRuntime {
    fence: ProbeSessionFence,
    executor: Mutex<ProbeExecutor<SettingsProbeCatalog>>,
    catalogs: Arc<Mutex<HashMap<ProbeKind, (ProbeToken, SettingsProbeCatalog)>>>,
    stop: Arc<AtomicBool>,
    result_pump: Mutex<Option<JoinHandle<()>>>,
}

#[allow(dead_code)]
impl SettingsProbeRuntime {
    pub fn new(sink: TauriUiEventSink) -> Result<Self, String> {
        let fence = ProbeSessionFence::new(
            ProbeSessionOwner::new(
                clipline_settings::SettingsSessionGeneration::new(0),
                clipline_settings::SettingsAttachmentGeneration::new(0),
                clipline_settings::SettingsForegroundGeneration::new(0),
            ),
            SettingsTab::General,
        );
        // No probe work is admitted before a concrete Settings window/session
        // attaches and selects a tab.
        fence.disconnect();
        let (executor, results) = ProbeExecutor::new(Arc::new(fence.clone()))?;
        let catalogs = Arc::new(Mutex::new(HashMap::with_capacity(ProbeKind::COUNT)));
        let stop = Arc::new(AtomicBool::new(false));
        let result_pump = spawn_result_pump(results, sink, catalogs.clone(), stop.clone())?;
        Ok(Self {
            fence,
            executor: Mutex::new(executor),
            catalogs,
            stop,
            result_pump: Mutex::new(Some(result_pump)),
        })
    }

    pub fn open_session(&self, owner: ProbeSessionOwner, active_tab: SettingsTab) {
        self.catalogs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        self.fence.replace_owner(owner, active_tab);
    }

    pub fn set_active_tab(&self, active_tab: SettingsTab) {
        self.catalogs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        self.fence.set_active_tab(active_tab);
    }

    pub fn disconnect(&self) {
        self.fence.disconnect();
        self.catalogs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    pub fn request(&self, kind: ProbeKind) -> Result<ProbeToken, String> {
        self.fence.request(kind).map_err(|error| error.to_string())
    }

    pub fn catalog(&self, token: ProbeToken) -> Option<SettingsProbeCatalog> {
        self.catalogs
            .lock()
            .ok()
            .and_then(|catalogs| catalogs.get(&token.kind).cloned())
            .and_then(|(stored, catalog)| (stored == token).then_some(catalog))
    }

    pub fn submit(
        &self,
        token: ProbeToken,
        settings: &AppSettings,
    ) -> Result<ProbeSubmitOutcome, String> {
        match token.kind {
            ProbeKind::Displays => self.submit_work(token, 0, |context| {
                clipline_capture::windows::display::enumerate_displays_with_checkpoint(|| {
                    context.checkpoint_after_activation()
                })
                .map(SettingsProbeCatalog::Displays)
                .map_err(|error| error.to_string())
            }),
            ProbeKind::AudioEndpoints => self.submit_work(token, 0, |context| {
                clipline_capture::windows::wasapi::enumerate_audio_devices_with_checkpoint(|| {
                    context.checkpoint_after_activation()
                })
                .map(SettingsProbeCatalog::AudioEndpoints)
                .map_err(|error| error.to_string())
            }),
            ProbeKind::Encoders => self.submit_work(token, 0, |context| {
                let encoders = clipline_recorder::probe::available_encoder_options_bounded()?;
                context.checkpoint_after_activation()?;
                Ok(SettingsProbeCatalog::Encoders(encoders))
            }),
            ProbeKind::GameWindows => self.submit_work(token, 0, |context| {
                clipline_games::windows::list_game_windows_with_checkpoint(|| {
                    context.checkpoint_after_activation()
                })
                .map(SettingsProbeCatalog::GameWindows)
            }),
            ProbeKind::InstalledGames => {
                let mut games = settings.games.custom_games.clone();
                for game in &mut games {
                    game.icon = None;
                    game.legacy_ids.clear();
                }
                let owned_bytes = custom_game_work_bytes(&games)?;
                self.submit_work(token, owned_bytes, move |context| {
                    clipline_games::windows::detect_installed_games_with_checkpoint(&games, || {
                        context.checkpoint_after_activation()
                    })
                    .map(SettingsProbeCatalog::InstalledGames)
                })
            }
            ProbeKind::GamePlugins => {
                let icon_cache = clipline_settings::icon_cache_dir();
                let owned_bytes = icon_cache.as_os_str().len();
                self.submit_work(token, owned_bytes, move |context| {
                    let _ = std::fs::metadata(&icon_cache);
                    context.checkpoint_after_activation()?;
                    clipline_games::plugin::catalog_bounded(&icon_cache)
                        .map(SettingsProbeCatalog::GamePlugins)
                })
            }
            ProbeKind::Storage => {
                let media_dir = settings.media_dir_path()?;
                let quota = quota_bytes_from_gb(settings.disk_quota_gb)?;
                let owned_bytes = media_dir.as_os_str().len();
                self.submit_work(token, owned_bytes, move |context| {
                    clipline_storage::storage_status_with_checkpoint(&media_dir, quota, || {
                        context.checkpoint_after_activation()
                    })
                    .map(SettingsProbeCatalog::Storage)
                })
            }
            ProbeKind::PlaybackCapabilities => self.submit_work(token, 0, |context| {
                context.checkpoint_after_activation()?;
                Err("native playback capability probe is not available until Task 6".into())
            }),
        }
    }

    fn submit_work(
        &self,
        token: ProbeToken,
        owned_work_bytes: usize,
        work: impl FnOnce(&ProbeExecutionContext) -> Result<SettingsProbeCatalog, String>
            + Send
            + 'static,
    ) -> Result<ProbeSubmitOutcome, String> {
        self.executor
            .lock()
            .map_err(|_| "settings probe executor lock poisoned".to_string())?
            .submit(token, owned_work_bytes, work)
            .map_err(|error| error.to_string())
    }
}

impl Drop for SettingsProbeRuntime {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        match self.executor.get_mut() {
            Ok(executor) => executor.shutdown(),
            Err(poisoned) => poisoned.into_inner().shutdown(),
        }
        let pump = match self.result_pump.get_mut() {
            Ok(pump) => pump.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(pump) = pump {
            let _ = pump.join();
        }
    }
}

fn spawn_result_pump(
    results: ProbeResultReceiver<SettingsProbeCatalog>,
    sink: TauriUiEventSink,
    catalogs: Arc<Mutex<HashMap<ProbeKind, (ProbeToken, SettingsProbeCatalog)>>>,
    stop: Arc<AtomicBool>,
) -> Result<JoinHandle<()>, String> {
    std::thread::Builder::new()
        .name("clipline-settings-probe-results".into())
        .spawn(move || {
            while !stop.load(Ordering::Acquire) {
                let Some(result) = results.wait_recv(RESULT_POLL) else {
                    continue;
                };
                let token = result.token;
                let summary = result.summary();
                {
                    let mut catalogs = catalogs
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    match result.outcome {
                        ProbeOutcome::Ready(catalog) => {
                            catalogs.insert(token.kind, (token, catalog));
                        }
                        ProbeOutcome::Failed(_) => {
                            catalogs.remove(&token.kind);
                        }
                    }
                }
                if let Err(error) = sink.try_publish(UiEvent::SettingsProbeChanged { summary }) {
                    let mut catalogs = catalogs
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if catalogs
                        .get(&token.kind)
                        .is_some_and(|(stored, _)| *stored == token)
                    {
                        catalogs.remove(&token.kind);
                    }
                    tracing::warn!(event = "settings_probe_result_publish_failed", error = %error);
                }
            }
        })
        .map_err(|error| format!("spawn settings probe result pump: {error}"))
}

fn custom_game_work_bytes(games: &[CustomGameSettings]) -> Result<usize, String> {
    let base = size_of::<CustomGameSettings>()
        .checked_mul(games.len())
        .ok_or_else(|| "custom game probe work byte count overflowed".to_string())?;
    games.iter().try_fold(base, |total, game| {
        [
            game.id.as_str(),
            game.name.as_str(),
            game.exe_name.as_str(),
            game.process_path.as_deref().unwrap_or_default(),
            game.window_title.as_str(),
        ]
        .into_iter()
        .try_fold(total, |total, value| {
            total
                .checked_add(value.len())
                .ok_or_else(|| "custom game probe work byte count overflowed".to_string())
        })
    })
}
