//! Checked tray-first lifecycle for the Slint shell.
//!
//! This module contains no Slint types. The UI-thread adapter owns concrete
//! components and media resources, while this state machine proves that only
//! one attachment may exist and that delayed callbacks cannot cross a drop /
//! rebuild boundary.

use std::fmt;

use clipline_shell::{
    LaunchMode, ShellCommand, ShellGeneration, WindowEvent, WindowMode, WindowPolicy,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachmentToken(ShellGeneration);

impl AttachmentToken {
    #[must_use]
    pub const fn generation(self) -> u64 {
        self.0.get()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellLabels {
    pub recorder: String,
    pub save_replay: String,
}

impl ShellLabels {
    #[must_use]
    pub fn new(recorder: impl Into<String>, save_replay: impl Into<String>) -> Self {
        Self {
            recorder: recorder.into(),
            save_replay: save_replay.into(),
        }
    }
}

impl Default for ShellLabels {
    fn default() -> Self {
        Self::new("BUFFERING", "Alt+F10  Save Replay")
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LifecycleCounters {
    pub open_requests: u64,
    pub close_requests: u64,
    pub windows_created: u64,
    pub windows_dropped: u64,
    pub stale_callbacks: u64,
    pub quit_effects: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LifecycleSnapshot {
    pub mode: WindowMode,
    pub window_active: bool,
    pub quitting: bool,
    pub attachment_generation: u64,
    pub counters: LifecycleCounters,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowTeardownStep {
    PublishBackground,
    StopWindowMedia,
    StopPlayback,
    ReleasePresentationResources,
    DetachDesktop,
    DropComponent,
}

pub const WINDOW_TEARDOWN_ORDER: [WindowTeardownStep; 6] = [
    WindowTeardownStep::PublishBackground,
    WindowTeardownStep::StopWindowMedia,
    WindowTeardownStep::StopPlayback,
    WindowTeardownStep::ReleasePresentationResources,
    WindowTeardownStep::DetachDesktop,
    WindowTeardownStep::DropComponent,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleAction {
    KeepTrayOnly,
    CreateWindow { attachment: AttachmentToken },
    RevealWindow { attachment: AttachmentToken },
    DropToTray { attachment: AttachmentToken },
    SaveReplay,
    OpenDiagnostics,
    Quit { attachment: Option<AttachmentToken> },
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowState {
    Absent,
    Creating(AttachmentToken),
    Active(AttachmentToken),
    Dropping(AttachmentToken),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleError {
    GenerationExhausted,
    CounterExhausted,
    StaleAttachment,
    UnexpectedWindowState,
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for LifecycleError {}

pub struct ShellLifecycle {
    policy: WindowPolicy,
    attachment_generation: ShellGeneration,
    window: WindowState,
    labels: ShellLabels,
    counters: LifecycleCounters,
}

impl ShellLifecycle {
    pub fn for_launch(mode: LaunchMode) -> Result<(Self, LifecycleAction), LifecycleError> {
        Self::for_launch_with_labels(mode, ShellLabels::default())
    }

    pub fn for_launch_with_labels(
        mode: LaunchMode,
        labels: ShellLabels,
    ) -> Result<(Self, LifecycleAction), LifecycleError> {
        let (policy, initial) = WindowPolicy::for_launch(mode);
        let mut lifecycle = Self {
            policy,
            attachment_generation: ShellGeneration::INITIAL,
            window: WindowState::Absent,
            labels,
            counters: LifecycleCounters::default(),
        };
        let action = match initial {
            clipline_shell::WindowEffect::KeepTrayOnly => LifecycleAction::KeepTrayOnly,
            clipline_shell::WindowEffect::CreateAndReveal => lifecycle.request_open()?,
            _ => return Err(LifecycleError::UnexpectedWindowState),
        };
        Ok((lifecycle, action))
    }

    pub fn handle_command(
        &mut self,
        command: ShellCommand,
    ) -> Result<LifecycleAction, LifecycleError> {
        match command {
            ShellCommand::Open => self.request_open(),
            ShellCommand::SaveReplay => Ok(LifecycleAction::SaveReplay),
            ShellCommand::OpenDiagnostics => Ok(LifecycleAction::OpenDiagnostics),
            ShellCommand::Quit => self.request_quit(),
            ShellCommand::CheckUpdates | ShellCommand::InstallUpdate => Ok(LifecycleAction::None),
        }
    }

    fn request_open(&mut self) -> Result<LifecycleAction, LifecycleError> {
        increment(&mut self.counters.open_requests)?;
        if self.policy.is_quitting() {
            return Ok(LifecycleAction::None);
        }
        match self.window {
            WindowState::Absent => {
                self.attachment_generation = self
                    .attachment_generation
                    .checked_next()
                    .map_err(|_| LifecycleError::GenerationExhausted)?;
                let attachment = AttachmentToken(self.attachment_generation);
                self.window = WindowState::Creating(attachment);
                let _ = self.policy.apply(WindowEvent::RevealRequested);
                Ok(LifecycleAction::CreateWindow { attachment })
            }
            WindowState::Active(attachment) => {
                let _ = self.policy.apply(WindowEvent::RevealRequested);
                Ok(LifecycleAction::RevealWindow { attachment })
            }
            WindowState::Creating(_) | WindowState::Dropping(_) => Ok(LifecycleAction::None),
        }
    }

    pub fn window_created(&mut self, attachment: AttachmentToken) -> Result<(), LifecycleError> {
        if self.window != WindowState::Creating(attachment) {
            return Err(LifecycleError::StaleAttachment);
        }
        increment(&mut self.counters.windows_created)?;
        self.window = WindowState::Active(attachment);
        Ok(())
    }

    pub fn window_create_failed(
        &mut self,
        attachment: AttachmentToken,
    ) -> Result<(), LifecycleError> {
        if self.window != WindowState::Creating(attachment) {
            return Err(LifecycleError::StaleAttachment);
        }
        self.window = WindowState::Absent;
        Ok(())
    }

    pub fn close_requested(
        &mut self,
        attachment: AttachmentToken,
    ) -> Result<LifecycleAction, LifecycleError> {
        increment(&mut self.counters.close_requests)?;
        if self.window != WindowState::Active(attachment) || self.policy.is_quitting() {
            increment(&mut self.counters.stale_callbacks)?;
            return Err(LifecycleError::StaleAttachment);
        }
        let _ = self.policy.apply(WindowEvent::CloseRequested);
        self.window = WindowState::Dropping(attachment);
        Ok(LifecycleAction::DropToTray { attachment })
    }

    pub fn window_dropped(&mut self, attachment: AttachmentToken) -> Result<(), LifecycleError> {
        if self.window != WindowState::Dropping(attachment) {
            return Err(LifecycleError::StaleAttachment);
        }
        increment(&mut self.counters.windows_dropped)?;
        self.window = WindowState::Absent;
        Ok(())
    }

    pub fn accept_callback(&mut self, attachment: AttachmentToken) -> Result<bool, LifecycleError> {
        if self.window == WindowState::Active(attachment) && !self.policy.is_quitting() {
            Ok(true)
        } else {
            increment(&mut self.counters.stale_callbacks)?;
            Ok(false)
        }
    }

    fn request_quit(&mut self) -> Result<LifecycleAction, LifecycleError> {
        if self.policy.is_quitting() {
            return Ok(LifecycleAction::None);
        }
        let _ = self.policy.apply(WindowEvent::QuitRequested);
        increment(&mut self.counters.quit_effects)?;
        let attachment = match self.window {
            WindowState::Creating(attachment) | WindowState::Active(attachment) => {
                self.window = WindowState::Dropping(attachment);
                Some(attachment)
            }
            WindowState::Dropping(attachment) => Some(attachment),
            WindowState::Absent => None,
        };
        Ok(LifecycleAction::Quit { attachment })
    }

    #[must_use]
    pub fn snapshot(&self) -> LifecycleSnapshot {
        LifecycleSnapshot {
            mode: self.policy.mode(),
            window_active: matches!(self.window, WindowState::Active(_)),
            quitting: self.policy.is_quitting(),
            attachment_generation: self.attachment_generation.get(),
            counters: self.counters,
        }
    }

    #[must_use]
    pub fn tray_labels(&self) -> &ShellLabels {
        &self.labels
    }

    #[must_use]
    pub fn window_labels(&self) -> &ShellLabels {
        &self.labels
    }
}

fn increment(counter: &mut u64) -> Result<(), LifecycleError> {
    *counter = counter
        .checked_add(1)
        .ok_or(LifecycleError::CounterExhausted)?;
    Ok(())
}

#[cfg(windows)]
mod windows_runtime {
    use std::cell::RefCell;
    use std::rc::{Rc, Weak};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use clipline_desktop::{
        Generation, RecorderEvent, Revision, UiEvent, WindowLifecycleMode, WindowLifecycleSnapshot,
    };
    use clipline_library::{
        catalog_result_channel, ActiveFileRegistry, CatalogDialogTextField, CatalogEffect,
        CatalogItemIdentity, CatalogResultReceiver, CatalogResultSender, CatalogSource,
        CatalogUploadVisibility, ForegroundGeneration, LocalClipFilter, LocalClipGrouping,
        LocalClipSort, PlaybackSourceLease, RequestGeneration, ValidatedClipPath,
        WindowAttachmentGeneration, WindowWorkToken, CATALOG_RESULT_CAPACITY,
    };
    use clipline_playback::windows::{WindowsD3D11Publisher, WindowsVideoHost};
    use clipline_settings::LibraryConfig;
    use clipline_shell::activation::ActivationCommand;
    use clipline_shell::windows::activation::{
        acquire_or_activate, WindowsInstanceGuard, WindowsInstanceRole,
    };
    use clipline_shell::windows::hotkey::WindowsHotkeyService;
    use clipline_shell::{
        shell_command_channel, LaunchMode, ShellCommand, ShellCommandReceiver, ShellCommandSender,
    };
    use slint::ComponentHandle;

    use crate::catalog::{
        publish_projection, CatalogEffectExecutor, CatalogResultWake, CatalogReviewPort,
        CatalogUiIntent, LocalCatalogEffectHandler, SlintCatalogController, SystemCatalogReveal,
        SystemDayResolver,
    };
    use crate::controller::PlaybackController;
    use crate::desktop::{DesktopAttachment, SlintDesktopAdapter};
    use crate::live::{
        LiveMediaCommandPort, LiveMediaLease, LiveMediaRequestToken, LiveSession,
        LiveSessionReport, SessionCommandPort, SpikePublisher, ValidatedLiveMediaSource,
    };
    use crate::options::{write_lifecycle_marker, SpikeOptions, SpikeScenario};
    use crate::poster::{
        clone_ui_poster, detach_ui_poster_window, replace_ui_poster_page, update_ui_poster_viewport,
    };
    use crate::settings::{CandidateSettings, CandidateSettingsProfile};
    use crate::windows::{attach_video_host, update_video_host};
    use crate::{
        CatalogDialogTextField as SlintDialogTextField, CatalogFilter as SlintCatalogFilter,
        CatalogGroup as SlintCatalogGroup, CatalogSort as SlintCatalogSort,
        CatalogSource as SlintCatalogSource,
        CatalogUploadVisibility as SlintCatalogUploadVisibility, CliplineSpike, DesktopUploadItem,
        LibraryItem, SpikeTray, TimelineMarker,
    };

    use super::{AttachmentToken, LifecycleAction, LifecycleSnapshot, ShellLifecycle};

    const PRODUCT_IDENTITY: &str = "io.clipline.app.slint-spike";
    const PACKAGE_INSTALL_FENCE_NAME: &str = r"Local\io.clipline.app.slint-candidate.package-fence";
    const COMMAND_POLL: Duration = Duration::from_millis(5);
    const HANDLE_RETRY: Duration = Duration::from_millis(10);
    const MAX_HANDLE_ATTEMPTS: u16 = 200;
    const REVEAL_CLOSE_CYCLES: u64 = 100;

    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct ShellResourceSnapshot {
        pub tray_ready: bool,
        pub desktop_consumer_alive: bool,
        pub hotkey_service_alive: bool,
        pub activation_service_alive: bool,
        pub max_live_windows: u64,
        pub desktop_attached: u64,
        pub desktop_detached: u64,
        pub playback_started: u64,
        pub playback_stopped: u64,
        pub video_hosts_created: u64,
        pub video_hosts_dropped: u64,
        pub live_desktop_attachments: u64,
        pub live_playback_sessions: u64,
        pub live_video_hosts: u64,
        pub model_sets_created: u64,
        pub model_sets_dropped: u64,
        pub live_model_sets: u64,
    }

    pub struct ShellRunReport {
        pub latest_session: Option<LiveSessionReport>,
        pub lifecycle: LifecycleSnapshot,
        pub resources: ShellResourceSnapshot,
    }

    struct WindowResources {
        attachment: AttachmentToken,
        catalog_attachment: WindowAttachmentGeneration,
        catalog_foreground: ForegroundGeneration,
        catalog_revision: u64,
        poster_token: WindowWorkToken,
        poster_stamp: Vec<(
            clipline_library::ClipPathIdentity,
            Option<clipline_shell::FileIdentity>,
        )>,
        poster_viewport_start: usize,
        desktop_attachment: DesktopAttachment,
        session: Option<LiveSession>,
        host: Option<WindowsVideoHost>,
        window: CliplineSpike,
    }

    struct SlintShell {
        lifecycle: ShellLifecycle,
        shell_commands: ShellCommandSender,
        shell_receiver: ShellCommandReceiver,
        activation: Option<WindowsInstanceGuard>,
        hotkeys: Option<WindowsHotkeyService>,
        tray: SpikeTray,
        desktop: SlintDesktopAdapter,
        _settings: CandidateSettings,
        catalog: SlintCatalogController,
        catalog_sender: CatalogResultSender,
        catalog_results: CatalogResultReceiver,
        catalog_executor: Option<CatalogEffectExecutor>,
        review_bridge: Arc<LiveReviewBridge>,
        pending_review_open: Option<CatalogEffect>,
        live_start_scheduled: Option<AttachmentToken>,
        window: Option<WindowResources>,
        options: SpikeOptions,
        latest_session: Option<LiveSessionReport>,
        resources: ShellResourceSnapshot,
        lifecycle_revision: Revision,
        marker_revision: u64,
        stop_observed: bool,
        quit_event_loop_requested: bool,
    }

    #[derive(Clone)]
    struct LiveReviewBinding {
        attachment: WindowAttachmentGeneration,
        foreground: ForegroundGeneration,
        port: LiveMediaCommandPort,
    }

    #[derive(Default)]
    struct LiveReviewBridge(Mutex<Option<LiveReviewBinding>>);

    impl LiveReviewBridge {
        fn attach(
            &self,
            attachment: WindowAttachmentGeneration,
            foreground: ForegroundGeneration,
            port: LiveMediaCommandPort,
        ) {
            *self.0.lock().unwrap_or_else(|poison| poison.into_inner()) = Some(LiveReviewBinding {
                attachment,
                foreground,
                port,
            });
        }

        fn revoke(&self, attachment: WindowAttachmentGeneration, foreground: ForegroundGeneration) {
            let mut binding = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
            if binding.as_ref().is_some_and(|binding| {
                binding.attachment == attachment && binding.foreground == foreground
            }) {
                binding.take();
            }
        }

        fn is_attached(
            &self,
            attachment: WindowAttachmentGeneration,
            foreground: ForegroundGeneration,
        ) -> bool {
            self.0
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .as_ref()
                .is_some_and(|binding| {
                    binding.attachment == attachment && binding.foreground == foreground
                })
        }
    }

    impl CatalogReviewPort for LiveReviewBridge {
        fn open(
            &self,
            token: WindowWorkToken,
            source: ValidatedClipPath,
            lease: PlaybackSourceLease,
        ) -> Result<(), String> {
            let binding = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
            let binding = binding
                .as_ref()
                .filter(|binding| {
                    binding.attachment == token.attachment && binding.foreground == token.foreground
                })
                .ok_or_else(|| "native review session belongs to a stale window".to_owned())?;
            let request = LiveMediaRequestToken::new(token.request.get())
                .map_err(|error| error.to_string())?;
            binding
                .port
                .open(
                    request,
                    ValidatedLiveMediaSource::local(source, LiveMediaLease::new(lease)),
                )
                .map(|_| ())
                .map_err(|error| error.to_string())
        }
    }

    #[derive(Default)]
    struct TimerCatalogWake;

    impl CatalogResultWake for TimerCatalogWake {
        fn wake(&self) {}
    }

    pub fn run(options: SpikeOptions) -> Result<ShellRunReport, String> {
        let (shell_commands, shell_receiver) = shell_command_channel();
        let activation_command = if options.autostart {
            ActivationCommand::AutostartNoop
        } else {
            ActivationCommand::Reveal
        };
        let activation =
            match acquire_or_activate(PRODUCT_IDENTITY, activation_command, shell_commands.clone())
                .map_err(|error| format!("start Slint spike activation service: {error}"))?
            {
                WindowsInstanceRole::Primary(guard) => guard,
                WindowsInstanceRole::Secondary(_) => {
                    return Ok(ShellRunReport {
                        latest_session: None,
                        lifecycle: ShellLifecycle::for_launch(LaunchMode::Autostart)
                            .map_err(|error| error.to_string())?
                            .0
                            .snapshot(),
                        resources: ShellResourceSnapshot::default(),
                    })
                }
            };
        let _package_install_fence =
            clipline_shell::windows::activation::WindowsProcessFence::acquire(
                PACKAGE_INSTALL_FENCE_NAME,
            )
            .map_err(|error| format!("acquire package install fence: {error}"))?;
        let settings = CandidateSettings::open(CandidateSettingsProfile::from_isolated_path(
            options.settings_profile.as_deref(),
        ))
        .map_err(|error| format!("open Slint candidate settings profile: {error}"))?;
        settings
            .snapshot()
            .map_err(|error| format!("open Slint candidate settings: {error}"))?;
        let media_root = settings
            .store()
            .media_root()
            .map_err(|error| format!("resolve Slint candidate media root: {error}"))?;
        std::fs::create_dir_all(&media_root)
            .map_err(|error| format!("create Slint candidate media root: {error}"))?;
        let active_files = ActiveFileRegistry::new();
        let review_bridge = Arc::new(LiveReviewBridge::default());
        let catalog_handler = Arc::new(LocalCatalogEffectHandler::open_with_review_port(
            &media_root,
            active_files,
            Arc::new(clipline_library::Mp4LegacyAudioTrackProbe),
            Arc::new(SystemCatalogReveal),
            review_bridge.clone(),
        )?);
        let (catalog_sender, catalog_results) = catalog_result_channel();
        let catalog_executor = CatalogEffectExecutor::start(
            catalog_handler,
            catalog_sender.clone(),
            Arc::new(TimerCatalogWake),
        )?;
        let catalog = SlintCatalogController::new(Arc::new(SystemDayResolver))
            .map_err(|error| error.to_string())?;
        let hotkeys = WindowsHotkeyService::start(shell_commands.clone())
            .map_err(|error| format!("start Slint spike hotkey service: {error}"))?;

        let tray = SpikeTray::new().map_err(|error| error.to_string())?;
        tray.set_tray_icon(tray_icon());
        let desktop = SlintDesktopAdapter::start_with_tray(tray.as_weak())?;
        publish_recorder_snapshot(&desktop)?;
        let launch_mode = if options.autostart {
            LaunchMode::Autostart
        } else {
            LaunchMode::Normal
        };
        let (lifecycle, initial_action) =
            ShellLifecycle::for_launch(launch_mode).map_err(|error| error.to_string())?;
        tray.set_save_replay_label(lifecycle.tray_labels().save_replay.clone().into());

        let runtime = Rc::new(RefCell::new(SlintShell {
            lifecycle,
            shell_commands,
            shell_receiver,
            activation: Some(activation),
            hotkeys: Some(hotkeys),
            tray,
            desktop,
            _settings: settings,
            catalog,
            catalog_sender,
            catalog_results,
            catalog_executor: Some(catalog_executor),
            review_bridge,
            pending_review_open: None,
            live_start_scheduled: None,
            window: None,
            options,
            latest_session: None,
            resources: ShellResourceSnapshot::default(),
            lifecycle_revision: Revision::INITIAL,
            marker_revision: 0,
            stop_observed: false,
            quit_event_loop_requested: false,
        }));
        wire_tray_callbacks(&runtime);
        runtime
            .borrow()
            .tray
            .show()
            .map_err(|error| error.to_string())?;
        {
            let mut runtime_ref = runtime.borrow_mut();
            runtime_ref.resources.tray_ready = true;
            runtime_ref.resources.desktop_consumer_alive = runtime_ref.desktop.consumer_alive();
            runtime_ref.resources.hotkey_service_alive = runtime_ref.hotkeys.is_some();
            runtime_ref.resources.activation_service_alive = runtime_ref.activation.is_some();
            write_shell_marker(
                &mut runtime_ref,
                "trayReady",
                "tray-first shell services ready",
            );
        }
        execute_action(&runtime, initial_action)?;
        if runtime.borrow().options.scenario == SpikeScenario::RevealClose100
            && runtime.borrow().window.is_none()
        {
            runtime
                .borrow()
                .shell_commands
                .try_send(ShellCommand::Open)
                .map_err(|error| error.to_string())?;
        }
        schedule_command_pump(Rc::downgrade(&runtime));
        slint::run_event_loop_until_quit().map_err(|error| error.to_string())?;
        shutdown_after_event_loop(&runtime)?;

        let mut runtime = Rc::try_unwrap(runtime)
            .map_err(|_| "Slint shell retained a strong runtime callback".to_owned())?
            .into_inner();
        // The report is serialized only after `runtime` is dropped below, so
        // describe the post-drop service state rather than the pre-drop guard.
        runtime.resources.desktop_consumer_alive = false;
        runtime.resources.tray_ready = false;
        let report = ShellRunReport {
            latest_session: runtime.latest_session.take(),
            lifecycle: runtime.lifecycle.snapshot(),
            resources: runtime.resources,
        };
        drop(runtime);
        Ok(report)
    }

    fn wire_tray_callbacks(runtime: &Rc<RefCell<SlintShell>>) {
        let sender = runtime.borrow().shell_commands.clone();
        runtime.borrow().tray.on_show_window(move || {
            let _ = sender.try_send(ShellCommand::Open);
        });
        let sender = runtime.borrow().shell_commands.clone();
        runtime.borrow().tray.on_save_replay(move || {
            let _ = sender.try_send(ShellCommand::SaveReplay);
        });
        let sender = runtime.borrow().shell_commands.clone();
        runtime.borrow().tray.on_open_diagnostics(move || {
            let _ = sender.try_send(ShellCommand::OpenDiagnostics);
        });
        let sender = runtime.borrow().shell_commands.clone();
        runtime.borrow().tray.on_quit_app(move || {
            let _ = sender.try_send(ShellCommand::Quit);
        });
    }

    fn schedule_command_pump(runtime: Weak<RefCell<SlintShell>>) {
        slint::Timer::single_shot(COMMAND_POLL, move || {
            let Some(runtime) = runtime.upgrade() else {
                return;
            };
            if let Err(error) = pump_commands(&runtime) {
                report_shell_error(&runtime, &error);
                let _ = dispatch_command(&runtime, ShellCommand::Quit);
            }
            if !runtime.borrow().quit_event_loop_requested {
                schedule_command_pump(Rc::downgrade(&runtime));
            }
        });
    }

    fn pump_commands(runtime: &Rc<RefCell<SlintShell>>) -> Result<(), String> {
        let should_stop = {
            let runtime = runtime.borrow();
            !runtime.stop_observed
                && runtime
                    .options
                    .stop_path
                    .as_ref()
                    .is_some_and(|path| path.exists())
        };
        if should_stop {
            runtime.borrow_mut().stop_observed = true;
            runtime
                .borrow()
                .shell_commands
                .try_send(ShellCommand::Quit)
                .map_err(|error| error.to_string())?;
        }
        loop {
            let command = runtime.borrow().shell_receiver.try_recv();
            let Some(command) = command else {
                break;
            };
            dispatch_command(runtime, command.command)?;
        }
        pump_catalog_results(runtime)?;
        Ok(())
    }

    fn pump_catalog_results(runtime: &Rc<RefCell<SlintShell>>) -> Result<(), String> {
        let mut changed = false;
        for _ in 0..CATALOG_RESULT_CAPACITY {
            let result = runtime.borrow().catalog_results.try_recv();
            let Some(result) = result else {
                break;
            };
            let (effects, result_changed) = {
                let mut runtime_ref = runtime.borrow_mut();
                let before = runtime_ref.catalog.revision();
                let effects = runtime_ref
                    .catalog
                    .accept(result)
                    .map_err(|error| error.to_string())?;
                (effects, runtime_ref.catalog.revision() != before)
            };
            changed |= result_changed;
            route_catalog_effects(runtime, effects)?;
        }
        if changed {
            publish_catalog_window(runtime)?;
        }
        Ok(())
    }

    fn publish_catalog_window(runtime: &Rc<RefCell<SlintShell>>) -> Result<(), String> {
        let (
            projection,
            poster_page,
            window,
            attachment,
            catalog_attachment,
            catalog_foreground,
            current_poster_stamp,
            poster_viewport_start,
            catalog_sender,
        ) = {
            let runtime_ref = runtime.borrow();
            let Some(resources) = runtime_ref.window.as_ref() else {
                return Ok(());
            };
            (
                runtime_ref.catalog.projection(),
                runtime_ref
                    .catalog
                    .poster_page()
                    .map_err(|error| error.to_string())?,
                resources.window.as_weak(),
                resources.attachment,
                resources.catalog_attachment,
                resources.catalog_foreground,
                resources.poster_stamp.clone(),
                resources.poster_viewport_start,
                runtime_ref.catalog_sender.clone(),
            )
        };
        if current_poster_stamp != poster_page.stamp {
            let poster_token = WindowWorkToken {
                attachment: catalog_attachment,
                foreground: catalog_foreground,
                request: RequestGeneration::new(projection.revision.get()),
            };
            replace_ui_poster_page(
                window.clone(),
                catalog_sender,
                poster_token,
                poster_page.items,
                poster_viewport_start,
            )?;
            let mut runtime_ref = runtime.borrow_mut();
            let Some(resources) = runtime_ref.window.as_mut() else {
                detach_ui_poster_window(poster_token)?;
                return Ok(());
            };
            if resources.attachment != attachment {
                detach_ui_poster_window(poster_token)?;
                return Ok(());
            }
            resources.poster_token = poster_token;
            resources.poster_stamp = poster_page.stamp;
        }
        let Some(window) = window.upgrade() else {
            return Ok(());
        };
        publish_projection(&window, projection.as_ref(), |identity| match identity {
            CatalogItemIdentity::Local { path } => clone_ui_poster(path),
            CatalogItemIdentity::Cloud { .. } => None,
        })
        .map_err(|error| error.to_string())?;
        let mut runtime_ref = runtime.borrow_mut();
        let Some(resources) = runtime_ref.window.as_mut() else {
            return Ok(());
        };
        if resources.attachment == attachment {
            resources.catalog_revision = projection.revision.get();
        }
        Ok(())
    }

    fn dispatch_catalog_intent(
        runtime: &Rc<RefCell<SlintShell>>,
        attachment: AttachmentToken,
        intent: CatalogUiIntent,
    ) -> Result<(), String> {
        let effects = {
            let mut runtime_ref = runtime.borrow_mut();
            if !runtime_ref
                .lifecycle
                .accept_callback(attachment)
                .map_err(|error| error.to_string())?
            {
                return Ok(());
            }
            let revision = runtime_ref
                .window
                .as_ref()
                .filter(|resources| resources.attachment == attachment)
                .map(|resources| resources.catalog_revision)
                .ok_or_else(|| "catalog callback belongs to a missing window".to_owned())?;
            runtime_ref
                .catalog
                .dispatch(revision, intent)
                .map_err(|error| error.to_string())?
        };
        publish_catalog_window(runtime)?;
        route_catalog_effects(runtime, effects)
    }

    fn route_catalog_effects(
        runtime: &Rc<RefCell<SlintShell>>,
        effects: Vec<CatalogEffect>,
    ) -> Result<(), String> {
        for effect in effects {
            match effect {
                CatalogEffect::CloseReview { token } => stop_review_session(runtime, token)?,
                CatalogEffect::ReleaseCloudReviewMedia { .. } => {}
                effect @ CatalogEffect::OpenLocalReview { token, .. }
                    if !runtime
                        .borrow()
                        .review_bridge
                        .is_attached(token.attachment, token.foreground) =>
                {
                    let attachment = {
                        let mut runtime_ref = runtime.borrow_mut();
                        if let Some(resources) = runtime_ref.window.as_ref() {
                            resources.window.set_review_visible(true);
                        }
                        runtime_ref.pending_review_open = Some(effect);
                        runtime_ref
                            .window
                            .as_ref()
                            .map(|resources| resources.attachment)
                    };
                    if let Some(attachment) = attachment {
                        schedule_live_start(Rc::downgrade(runtime), attachment, 0);
                    }
                }
                CatalogEffect::RefreshLocal { .. }
                | CatalogEffect::LoadClipDetail { .. }
                | CatalogEffect::OpenLocalReview { .. }
                | CatalogEffect::RenameTitle { .. }
                | CatalogEffect::RenameFile { .. }
                | CatalogEffect::Delete { .. }
                | CatalogEffect::Reveal { .. } => {
                    if matches!(&effect, CatalogEffect::OpenLocalReview { .. }) {
                        if let Some(resources) = runtime.borrow().window.as_ref() {
                            resources.window.set_review_visible(true);
                        }
                    }
                    let rejected = {
                        let runtime_ref = runtime.borrow();
                        let executor = runtime_ref
                            .catalog_executor
                            .as_ref()
                            .ok_or_else(|| "catalog executor is shut down".to_owned())?;
                        executor.try_submit(effect).err()
                    };
                    if let Some(rejected) = rejected {
                        if let Some(completion) = crate::catalog::rejected_effect_result(
                            &rejected,
                            "catalog executor queue is full",
                        ) {
                            let followups = runtime
                                .borrow_mut()
                                .catalog
                                .accept(completion.result)
                                .map_err(|error| error.to_string())?;
                            publish_catalog_window(runtime)?;
                            route_catalog_effects(runtime, followups)?;
                        }
                    }
                }
                unsupported => {
                    let runtime_ref = runtime.borrow();
                    if let Some(resources) = runtime_ref.window.as_ref() {
                        resources.window.set_status_text(
                            format!("Catalog action is not connected yet: {unsupported:?}").into(),
                        );
                    }
                }
            }
        }
        Ok(())
    }

    fn dispatch_command(
        runtime: &Rc<RefCell<SlintShell>>,
        command: ShellCommand,
    ) -> Result<(), String> {
        let action = runtime
            .borrow_mut()
            .lifecycle
            .handle_command(command)
            .map_err(|error| error.to_string())?;
        execute_action(runtime, action)
    }

    fn execute_action(
        runtime: &Rc<RefCell<SlintShell>>,
        action: LifecycleAction,
    ) -> Result<(), String> {
        match action {
            LifecycleAction::KeepTrayOnly | LifecycleAction::None => Ok(()),
            LifecycleAction::CreateWindow { attachment } => create_window(runtime, attachment),
            LifecycleAction::RevealWindow { attachment } => {
                let window = {
                    let runtime_ref = runtime.borrow();
                    let Some(resources) = runtime_ref.window.as_ref() else {
                        return Err("lifecycle requested reveal without a window".into());
                    };
                    if resources.attachment != attachment {
                        return Err("lifecycle requested reveal for a stale window".into());
                    }
                    resources.window.as_weak()
                };
                window
                    .upgrade()
                    .ok_or_else(|| "revealed window was dropped before Show".to_owned())?
                    .show()
                    .map_err(|error| error.to_string())
            }
            LifecycleAction::DropToTray { attachment } => drop_window(runtime, attachment),
            LifecycleAction::SaveReplay => {
                let mut runtime_ref = runtime.borrow_mut();
                write_shell_marker(
                    &mut runtime_ref,
                    "saveReplay",
                    "save replay command accepted by tray-first shell",
                );
                Ok(())
            }
            LifecycleAction::OpenDiagnostics => {
                clipline_shell::windows::shell_execute::open_folder(
                    &std::env::temp_dir(),
                    "open Clipline Slint spike diagnostics folder",
                )
                .map_err(|error| error.to_string())
            }
            LifecycleAction::Quit { attachment } => quit(runtime, attachment),
        }
    }

    fn create_window(
        runtime: &Rc<RefCell<SlintShell>>,
        attachment: AttachmentToken,
    ) -> Result<(), String> {
        if runtime.borrow().window.is_some() {
            return Err("window factory called while a window is already owned".into());
        }
        let window = match crate::create_window() {
            Ok(window) => window,
            Err(error) => {
                runtime
                    .borrow_mut()
                    .lifecycle
                    .window_create_failed(attachment)
                    .map_err(|failure| failure.to_string())?;
                return Err(error.to_string());
            }
        };
        {
            let runtime_ref = runtime.borrow();
            window.set_cpu_frame_diagnostic(runtime_ref.options.cpu_frame_diagnostic);
            window.set_save_replay_label(
                runtime_ref
                    .lifecycle
                    .window_labels()
                    .save_replay
                    .clone()
                    .into(),
            );
        }
        let desktop_result = { runtime.borrow().desktop.attach(window.as_weak()) };
        let desktop_attachment = match desktop_result {
            Ok(attachment) => attachment,
            Err(error) => {
                runtime
                    .borrow_mut()
                    .lifecycle
                    .window_create_failed(attachment)
                    .map_err(|failure| failure.to_string())?;
                return Err(error.to_string());
            }
        };
        wire_window_callbacks(runtime, &window, attachment);
        if let Err(error) = window.show() {
            let _ = runtime.borrow().desktop.detach(desktop_attachment);
            runtime
                .borrow_mut()
                .lifecycle
                .window_create_failed(attachment)
                .map_err(|failure| failure.to_string())?;
            return Err(error.to_string());
        }
        let catalog_effects = {
            let mut runtime_ref = runtime.borrow_mut();
            runtime_ref
                .lifecycle
                .window_created(attachment)
                .map_err(|error| error.to_string())?;
            checked_increment(&mut runtime_ref.resources.desktop_attached)?;
            runtime_ref.resources.live_desktop_attachments = 1;
            checked_increment(&mut runtime_ref.resources.model_sets_created)?;
            runtime_ref.resources.live_model_sets = 1;
            runtime_ref.resources.max_live_windows = 1;
            let catalog_attachment = WindowAttachmentGeneration::new(attachment.generation());
            let catalog_foreground = ForegroundGeneration::new(1);
            runtime_ref.window = Some(WindowResources {
                attachment,
                catalog_attachment,
                catalog_foreground,
                catalog_revision: 0,
                poster_token: WindowWorkToken {
                    attachment: catalog_attachment,
                    foreground: catalog_foreground,
                    request: RequestGeneration::INITIAL,
                },
                poster_stamp: Vec::new(),
                poster_viewport_start: 0,
                desktop_attachment,
                session: None,
                host: None,
                window,
            });
            publish_lifecycle(&mut runtime_ref, WindowLifecycleMode::Foreground)?;
            let catalog_attachment = WindowAttachmentGeneration::new(attachment.generation());
            let catalog_foreground =
                ForegroundGeneration::new(runtime_ref.lifecycle_revision.get());
            let effects = runtime_ref
                .catalog
                .attach(catalog_attachment, catalog_foreground)
                .map_err(|error| error.to_string())?;
            let projection = runtime_ref.catalog.projection();
            let resources = runtime_ref
                .window
                .as_mut()
                .expect("window was installed before catalog attach");
            publish_projection(&resources.window, projection.as_ref(), |_| None)
                .map_err(|error| error.to_string())?;
            resources.catalog_attachment = catalog_attachment;
            resources.catalog_foreground = catalog_foreground;
            resources.catalog_revision = projection.revision.get();
            write_shell_marker(
                &mut runtime_ref,
                "windowCreated",
                &format!("window attachment {} created", attachment.generation()),
            );
            effects
        };
        route_catalog_effects(runtime, catalog_effects)?;
        schedule_live_start(Rc::downgrade(runtime), attachment, 0);
        Ok(())
    }

    fn wire_window_callbacks(
        runtime: &Rc<RefCell<SlintShell>>,
        window: &CliplineSpike,
        attachment: AttachmentToken,
    ) {
        macro_rules! catalog_no_arg {
            ($setter:ident, $intent:expr) => {{
                let weak_runtime = Rc::downgrade(runtime);
                window.$setter(move || {
                    invoke_catalog_callback(&weak_runtime, attachment, $intent);
                });
            }};
        }
        macro_rules! catalog_row {
            ($setter:ident, $intent:ident) => {{
                let weak_runtime = Rc::downgrade(runtime);
                window.$setter(move |row| {
                    let Ok(row) = usize::try_from(row) else {
                        return;
                    };
                    invoke_catalog_callback(
                        &weak_runtime,
                        attachment,
                        CatalogUiIntent::$intent { row },
                    );
                });
            }};
        }
        let weak_runtime = Rc::downgrade(runtime);
        window.on_play_pause(move || {
            with_controller(&weak_runtime, attachment, |controller| {
                controller.play_pause()
            });
        });
        let weak_runtime = Rc::downgrade(runtime);
        window.on_seek(move |seconds| {
            with_controller(&weak_runtime, attachment, |controller| {
                controller.seek_relative(f64::from(seconds))
            });
        });
        let weak_runtime = Rc::downgrade(runtime);
        window.on_set_track(move |track, selected| {
            let Ok(track) = usize::try_from(track) else {
                return;
            };
            with_controller(&weak_runtime, attachment, |controller| {
                controller.set_track(track, selected)
            });
        });
        let weak_runtime = Rc::downgrade(runtime);
        window.on_set_volume(move |volume| {
            with_controller(&weak_runtime, attachment, |controller| {
                controller.set_volume(volume)
            });
        });
        let weak_runtime = Rc::downgrade(runtime);
        window.on_video_geometry_changed(move || {
            let Some(runtime) = weak_runtime.upgrade() else {
                return;
            };
            let mut runtime_ref = runtime.borrow_mut();
            if !runtime_ref
                .lifecycle
                .accept_callback(attachment)
                .unwrap_or(false)
            {
                return;
            }
            let Some(resources) = runtime_ref.window.as_mut() else {
                return;
            };
            if let Some(host) = resources.host.as_mut() {
                if let Err(error) = update_video_host(host, &resources.window) {
                    resources.window.set_status_text(error.into());
                }
            }
        });
        let weak_runtime = Rc::downgrade(runtime);
        window.on_show_library(move || {
            invoke_catalog_callback(&weak_runtime, attachment, CatalogUiIntent::CloseActive);
        });
        window.on_show_review(|| {});

        let weak_runtime = Rc::downgrade(runtime);
        window.on_catalog_set_source(move |source| {
            let source = match source {
                SlintCatalogSource::Local => CatalogSource::Local,
                SlintCatalogSource::Cloud => CatalogSource::Cloud,
            };
            invoke_catalog_callback(
                &weak_runtime,
                attachment,
                CatalogUiIntent::SetSource(source),
            );
        });
        let weak_runtime = Rc::downgrade(runtime);
        window.on_catalog_set_query(move |query| {
            invoke_catalog_callback(
                &weak_runtime,
                attachment,
                CatalogUiIntent::SetQuery(query.to_string()),
            );
        });
        let weak_runtime = Rc::downgrade(runtime);
        window.on_catalog_set_filter(move |filter| {
            let filter = match filter {
                SlintCatalogFilter::All => LocalClipFilter::All,
                SlintCatalogFilter::Replay => LocalClipFilter::Replay,
                SlintCatalogFilter::Session => LocalClipFilter::Session,
                SlintCatalogFilter::Trim => LocalClipFilter::Trim,
                SlintCatalogFilter::Marked => LocalClipFilter::Marked,
            };
            invoke_catalog_callback(
                &weak_runtime,
                attachment,
                CatalogUiIntent::SetLocalFilter(filter),
            );
        });
        let weak_runtime = Rc::downgrade(runtime);
        window.on_catalog_set_sort(move |sort| {
            let sort = match sort {
                SlintCatalogSort::Newest => LocalClipSort::Newest,
                SlintCatalogSort::Oldest => LocalClipSort::Oldest,
                SlintCatalogSort::Largest => LocalClipSort::Largest,
                SlintCatalogSort::Marks => LocalClipSort::Marks,
            };
            invoke_catalog_callback(
                &weak_runtime,
                attachment,
                CatalogUiIntent::SetLocalSort(sort),
            );
        });
        let weak_runtime = Rc::downgrade(runtime);
        window.on_catalog_set_group(move |group| {
            let grouping = match group {
                SlintCatalogGroup::Smart => LocalClipGrouping::Smart,
                SlintCatalogGroup::Day => LocalClipGrouping::Day,
                SlintCatalogGroup::Game => LocalClipGrouping::Game,
                SlintCatalogGroup::Session => LocalClipGrouping::Session,
                SlintCatalogGroup::Ungrouped => LocalClipGrouping::None,
            };
            invoke_catalog_callback(
                &weak_runtime,
                attachment,
                CatalogUiIntent::SetLocalGrouping(grouping),
            );
        });
        catalog_no_arg!(on_catalog_previous_page, CatalogUiIntent::PreviousPage);
        catalog_no_arg!(on_catalog_next_page, CatalogUiIntent::NextPage);
        catalog_no_arg!(on_catalog_refresh, CatalogUiIntent::Refresh);
        catalog_row!(on_catalog_open_row, OpenRow);
        catalog_row!(on_catalog_toggle_row, ToggleSelection);
        catalog_row!(on_catalog_open_context, OpenContext);
        catalog_no_arg!(on_catalog_enter_selection, CatalogUiIntent::EnterSelection);
        catalog_no_arg!(on_catalog_exit_selection, CatalogUiIntent::ExitSelection);
        catalog_no_arg!(
            on_catalog_select_visible,
            CatalogUiIntent::SelectVisiblePage
        );
        catalog_no_arg!(on_catalog_clear_selection, CatalogUiIntent::ClearSelection);
        catalog_no_arg!(
            on_catalog_delete_selection,
            CatalogUiIntent::DeleteSelection
        );
        catalog_no_arg!(on_catalog_escape, CatalogUiIntent::Escape);
        catalog_no_arg!(on_catalog_reveal_context, CatalogUiIntent::RevealFromMenu);
        catalog_no_arg!(on_catalog_upload_context, CatalogUiIntent::UploadFromMenu);
        catalog_no_arg!(
            on_catalog_rename_title_context,
            CatalogUiIntent::RenameTitleFromMenu
        );
        catalog_no_arg!(
            on_catalog_rename_file_context,
            CatalogUiIntent::RenameFileFromMenu
        );
        catalog_no_arg!(on_catalog_delete_context, CatalogUiIntent::DeleteFromMenu);
        catalog_no_arg!(
            on_catalog_cancel_upload_context,
            CatalogUiIntent::CancelUploadFromMenu
        );
        catalog_no_arg!(
            on_catalog_copy_link_context,
            CatalogUiIntent::CopyPublicLinkFromMenu
        );
        catalog_no_arg!(
            on_catalog_open_link_context,
            CatalogUiIntent::OpenInBrowserFromMenu
        );
        catalog_no_arg!(on_catalog_confirm_dialog, CatalogUiIntent::ConfirmDialog);
        catalog_no_arg!(on_catalog_cancel_dialog, CatalogUiIntent::CancelDialog);
        let weak_runtime = Rc::downgrade(runtime);
        window.on_catalog_set_dialog_text(move |field, value| {
            let field = match field {
                SlintDialogTextField::Title => CatalogDialogTextField::Title,
                SlintDialogTextField::FileName => CatalogDialogTextField::FileName,
                SlintDialogTextField::Description => CatalogDialogTextField::Description,
            };
            invoke_catalog_callback(
                &weak_runtime,
                attachment,
                CatalogUiIntent::SetDialogText {
                    field,
                    value: value.to_string(),
                },
            );
        });
        let weak_runtime = Rc::downgrade(runtime);
        window.on_catalog_set_upload_visibility(move |visibility| {
            let visibility = match visibility {
                SlintCatalogUploadVisibility::Private => CatalogUploadVisibility::Private,
                SlintCatalogUploadVisibility::Public => CatalogUploadVisibility::Public,
                SlintCatalogUploadVisibility::Unlisted => CatalogUploadVisibility::Unlisted,
            };
            invoke_catalog_callback(
                &weak_runtime,
                attachment,
                CatalogUiIntent::SetUploadVisibility(visibility),
            );
        });
        let weak_runtime = Rc::downgrade(runtime);
        window.on_catalog_set_audio_track(move |row, selected| {
            let Ok(row) = usize::try_from(row) else {
                return;
            };
            invoke_catalog_callback(
                &weak_runtime,
                attachment,
                CatalogUiIntent::SetUploadAudioTrack { row, selected },
            );
        });
        let weak_runtime = Rc::downgrade(runtime);
        window.on_catalog_poster_viewport(move |start| {
            let Ok(start) = usize::try_from(start) else {
                return;
            };
            let Some(runtime) = weak_runtime.upgrade() else {
                return;
            };
            let poster_token = {
                let mut runtime_ref = runtime.borrow_mut();
                if !runtime_ref
                    .lifecycle
                    .accept_callback(attachment)
                    .unwrap_or(false)
                {
                    return;
                }
                let Some(resources) = runtime_ref
                    .window
                    .as_mut()
                    .filter(|resources| resources.attachment == attachment)
                else {
                    return;
                };
                resources.poster_viewport_start = start;
                resources.poster_token
            };
            if let Err(error) = update_ui_poster_viewport(poster_token, start) {
                report_shell_error(&runtime, &error);
            }
        });

        let weak_runtime = Rc::downgrade(runtime);
        window.window().on_close_requested(move || {
            let weak_runtime = weak_runtime.clone();
            slint::Timer::single_shot(Duration::ZERO, move || {
                let Some(runtime) = weak_runtime.upgrade() else {
                    return;
                };
                let result = runtime
                    .borrow_mut()
                    .lifecycle
                    .close_requested(attachment)
                    .map_err(|error| error.to_string());
                match result {
                    Ok(action) => {
                        if let Err(error) = execute_action(&runtime, action) {
                            report_shell_error(&runtime, &error);
                        }
                    }
                    Err(error) => report_shell_error(&runtime, &error),
                }
            });
            slint::CloseRequestResponse::KeepWindowShown
        });
    }

    fn invoke_catalog_callback(
        runtime: &Weak<RefCell<SlintShell>>,
        attachment: AttachmentToken,
        intent: CatalogUiIntent,
    ) {
        let Some(runtime) = runtime.upgrade() else {
            return;
        };
        if let Err(error) = dispatch_catalog_intent(&runtime, attachment, intent) {
            report_shell_error(&runtime, &error);
        }
    }

    fn schedule_live_start(
        runtime: Weak<RefCell<SlintShell>>,
        attachment: AttachmentToken,
        attempt: u16,
    ) {
        let Some(runtime_now) = runtime.upgrade() else {
            return;
        };
        {
            let mut runtime_ref = runtime_now.borrow_mut();
            if runtime_ref.live_start_scheduled == Some(attachment) {
                return;
            }
            runtime_ref.live_start_scheduled = Some(attachment);
        }
        drop(runtime_now);
        slint::Timer::single_shot(HANDLE_RETRY, move || {
            let Some(runtime) = runtime.upgrade() else {
                return;
            };
            {
                let mut runtime_ref = runtime.borrow_mut();
                if runtime_ref.live_start_scheduled == Some(attachment) {
                    runtime_ref.live_start_scheduled = None;
                }
            }
            if let Err(error) = start_live(&runtime, attachment, attempt) {
                report_shell_error(&runtime, &error);
            }
        });
    }

    fn start_live(
        runtime: &Rc<RefCell<SlintShell>>,
        attachment: AttachmentToken,
        attempt: u16,
    ) -> Result<(), String> {
        {
            let mut runtime_ref = runtime.borrow_mut();
            if !runtime_ref
                .lifecycle
                .accept_callback(attachment)
                .map_err(|error| error.to_string())?
            {
                return Ok(());
            }
        }
        let (fixture, cpu_diagnostic, scenario, marker_path, exit_after_ready, window) = {
            let runtime_ref = runtime.borrow();
            let Some(resources) = runtime_ref.window.as_ref() else {
                return Ok(());
            };
            (
                runtime_ref.options.fixture.clone(),
                runtime_ref.options.cpu_frame_diagnostic,
                runtime_ref.options.scenario,
                runtime_ref.options.marker_path.clone(),
                runtime_ref.options.exit_after_ready,
                resources.window.as_weak(),
            )
        };
        let dynamic = fixture.is_none();
        let Some(window_component) = window.upgrade() else {
            return Ok(());
        };
        if !dynamic {
            window_component.set_review_visible(true);
        }
        let (publisher, host) = if cpu_diagnostic {
            (
                SpikePublisher::Cpu(crate::cpu_frame::CpuDiagnosticPublisher::new(window)),
                None,
            )
        } else {
            let (mut host, target) = match attach_video_host(window_component.window()) {
                Ok(value) => value,
                Err(_) if attempt + 1 < MAX_HANDLE_ATTEMPTS => {
                    window_component.set_status_text("Waiting for Slint Win32 handle".into());
                    schedule_live_start(Rc::downgrade(runtime), attachment, attempt + 1);
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
            update_video_host(&mut host, &window_component)?;
            (
                SpikePublisher::D3d(WindowsD3D11Publisher::new(target)),
                Some(host),
            )
        };
        let live_exit_after_ready = exit_after_ready && scenario != SpikeScenario::RevealClose100;
        let shell_commands = runtime.borrow().shell_commands.clone();
        let session = if let Some(fixture) = fixture {
            LiveSession::start(
                publisher,
                window_component.as_weak(),
                fixture,
                scenario,
                marker_path,
                live_exit_after_ready,
                shell_commands,
            )?
        } else {
            LiveSession::start_dynamic(
                publisher,
                window_component.as_weak(),
                scenario,
                marker_path,
                false,
                shell_commands,
            )?
        };
        let media_commands = session.media_commands();
        let host_created = host.is_some();
        let pending_review = {
            let mut runtime_ref = runtime.borrow_mut();
            if !runtime_ref
                .lifecycle
                .accept_callback(attachment)
                .map_err(|error| error.to_string())?
            {
                drop(session);
                drop(host);
                return Ok(());
            }
            let review_bridge = Arc::clone(&runtime_ref.review_bridge);
            let (catalog_attachment, catalog_foreground) = {
                let Some(resources) = runtime_ref.window.as_mut() else {
                    return Ok(());
                };
                resources.host = host;
                resources.session = Some(session);
                (resources.catalog_attachment, resources.catalog_foreground)
            };
            review_bridge.attach(catalog_attachment, catalog_foreground, media_commands);
            checked_increment(&mut runtime_ref.resources.playback_started)?;
            runtime_ref.resources.live_playback_sessions = 1;
            if host_created {
                checked_increment(&mut runtime_ref.resources.video_hosts_created)?;
                runtime_ref.resources.live_video_hosts = 1;
            }
            runtime_ref.pending_review_open.take()
        };
        if let Some(effect) = pending_review {
            route_catalog_effects(runtime, vec![effect])?;
        }
        if dynamic {
            window_component
                .set_status_text("Static Slint shell ready · native review idle".into());
            {
                let mut runtime_ref = runtime.borrow_mut();
                write_shell_marker(
                    &mut runtime_ref,
                    "ready",
                    "interactive Slint shell and native review runtime ready",
                );
            }
            if exit_after_ready {
                runtime
                    .borrow()
                    .shell_commands
                    .try_send(ShellCommand::Quit)
                    .map_err(|error| error.to_string())?;
            }
        }
        if scenario == SpikeScenario::RevealClose100 {
            let weak_runtime = Rc::downgrade(runtime);
            slint::Timer::single_shot(HANDLE_RETRY, move || {
                let Some(runtime) = weak_runtime.upgrade() else {
                    return;
                };
                let action = runtime
                    .borrow_mut()
                    .lifecycle
                    .close_requested(attachment)
                    .map_err(|error| error.to_string());
                match action {
                    Ok(action) => {
                        if let Err(error) = execute_action(&runtime, action) {
                            report_shell_error(&runtime, &error);
                        }
                    }
                    Err(error) => report_shell_error(&runtime, &error),
                }
            });
        }
        Ok(())
    }

    fn stop_review_session(
        runtime: &Rc<RefCell<SlintShell>>,
        token: WindowWorkToken,
    ) -> Result<(), String> {
        let (session, host) = {
            let mut runtime_ref = runtime.borrow_mut();
            let Some(resources) = runtime_ref.window.as_ref() else {
                return Ok(());
            };
            if resources.catalog_attachment != token.attachment
                || resources.catalog_foreground != token.foreground
            {
                return Ok(());
            }
            let catalog_attachment = resources.catalog_attachment;
            let catalog_foreground = resources.catalog_foreground;
            runtime_ref.pending_review_open = None;
            runtime_ref
                .review_bridge
                .revoke(catalog_attachment, catalog_foreground);
            let resources = runtime_ref
                .window
                .as_mut()
                .expect("review window was validated without yielding");
            resources.window.set_review_visible(false);
            (resources.session.take(), resources.host.take())
        };
        let mut first_error = None;
        if let Some(session) = session {
            match session.shutdown() {
                Ok(report) => runtime.borrow_mut().latest_session = Some(report),
                Err(error) => remember_first_error(&mut first_error, error),
            }
            let mut runtime_ref = runtime.borrow_mut();
            checked_increment(&mut runtime_ref.resources.playback_stopped)?;
            runtime_ref.resources.live_playback_sessions = 0;
        }
        if let Some(mut host) = host {
            if let Err(error) = host.close() {
                remember_first_error(&mut first_error, error.to_string());
            }
            let mut runtime_ref = runtime.borrow_mut();
            checked_increment(&mut runtime_ref.resources.video_hosts_dropped)?;
            runtime_ref.resources.live_video_hosts = 0;
        }
        first_error.map_or(Ok(()), Err)
    }

    fn drop_window(
        runtime: &Rc<RefCell<SlintShell>>,
        attachment: AttachmentToken,
    ) -> Result<(), String> {
        {
            let runtime_ref = runtime.borrow();
            let Some(resources) = runtime_ref.window.as_ref() else {
                return Err("window teardown requested with no owned window".into());
            };
            if resources.attachment != attachment {
                return Err("window teardown requested for a stale attachment".into());
            }
        }
        {
            let mut runtime_ref = runtime.borrow_mut();
            publish_lifecycle(&mut runtime_ref, WindowLifecycleMode::Tray)?;
            let resources = runtime_ref
                .window
                .as_ref()
                .expect("window was validated without yielding the UI thread");
            resources.window.set_playing(false);
        }
        let catalog_effects = {
            let mut runtime_ref = runtime.borrow_mut();
            let (catalog_attachment, catalog_foreground) = runtime_ref
                .window
                .as_ref()
                .map(|resources| (resources.catalog_attachment, resources.catalog_foreground))
                .ok_or_else(|| "catalog detach requested without a window".to_owned())?;
            runtime_ref.pending_review_open = None;
            runtime_ref.live_start_scheduled = None;
            runtime_ref
                .review_bridge
                .revoke(catalog_attachment, catalog_foreground);
            runtime_ref
                .catalog
                .detach()
                .map_err(|error| error.to_string())?
        };
        route_catalog_effects(runtime, catalog_effects)?;
        let poster_token = runtime
            .borrow()
            .window
            .as_ref()
            .map(|resources| resources.poster_token)
            .ok_or_else(|| "poster detach requested without a window".to_owned())?;
        detach_ui_poster_window(poster_token)?;
        let mut resources = runtime
            .borrow_mut()
            .window
            .take()
            .ok_or_else(|| "window disappeared during teardown".to_owned())?;
        let mut first_error = None;
        if let Some(session) = resources.session.take() {
            let report = session.shutdown();
            let mut runtime_ref = runtime.borrow_mut();
            if let Err(error) = checked_increment(&mut runtime_ref.resources.playback_stopped) {
                remember_first_error(&mut first_error, error);
            }
            runtime_ref.resources.live_playback_sessions = 0;
            match report {
                Ok(report) => runtime_ref.latest_session = Some(report),
                Err(error) => remember_first_error(&mut first_error, error),
            }
        }
        if let Some(mut host) = resources.host.take() {
            if let Err(error) = host.close() {
                remember_first_error(&mut first_error, error.to_string());
            }
            drop(host);
            let mut runtime_ref = runtime.borrow_mut();
            if let Err(error) = checked_increment(&mut runtime_ref.resources.video_hosts_dropped) {
                remember_first_error(&mut first_error, error);
            }
            runtime_ref.resources.live_video_hosts = 0;
        }
        clear_window_models(&resources.window);
        {
            let mut runtime_ref = runtime.borrow_mut();
            if let Err(error) = checked_increment(&mut runtime_ref.resources.model_sets_dropped) {
                remember_first_error(&mut first_error, error);
            }
            runtime_ref.resources.live_model_sets = 0;
        }
        let desktop_detached = runtime
            .borrow()
            .desktop
            .detach(resources.desktop_attachment);
        if desktop_detached.is_ok() {
            let mut runtime_ref = runtime.borrow_mut();
            if let Err(error) = checked_increment(&mut runtime_ref.resources.desktop_detached) {
                remember_first_error(&mut first_error, error);
            }
            runtime_ref.resources.live_desktop_attachments = 0;
        } else if let Err(error) = desktop_detached {
            remember_first_error(&mut first_error, error.to_string());
        }
        if let Err(error) = resources.window.hide() {
            remember_first_error(&mut first_error, error.to_string());
        }
        drop(resources);
        let lifecycle_result = {
            let mut runtime_ref = runtime.borrow_mut();
            let result = runtime_ref
                .lifecycle
                .window_dropped(attachment)
                .map_err(|error| error.to_string());
            if result.is_ok() {
                write_shell_marker(
                    &mut runtime_ref,
                    "windowDropped",
                    &format!("window attachment {} dropped", attachment.generation()),
                );
            }
            result
        };
        if let Err(error) = lifecycle_result {
            remember_first_error(&mut first_error, error);
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        let (dropped, scenario, exit_after_ready, sender) = {
            let runtime_ref = runtime.borrow();
            (
                runtime_ref.lifecycle.snapshot().counters.windows_dropped,
                runtime_ref.options.scenario,
                runtime_ref.options.exit_after_ready,
                runtime_ref.shell_commands.clone(),
            )
        };
        if scenario == SpikeScenario::RevealClose100 {
            if dropped < REVEAL_CLOSE_CYCLES {
                sender
                    .try_send(ShellCommand::Open)
                    .map_err(|error| error.to_string())?;
            } else if dropped == REVEAL_CLOSE_CYCLES {
                {
                    let mut runtime_ref = runtime.borrow_mut();
                    write_shell_marker(
                        &mut runtime_ref,
                        "ready",
                        "reveal-close-100 completed 100 real Slint create/drop cycles",
                    );
                }
                if exit_after_ready {
                    sender
                        .try_send(ShellCommand::Quit)
                        .map_err(|error| error.to_string())?;
                }
            }
        }
        Ok(())
    }

    fn quit(
        runtime: &Rc<RefCell<SlintShell>>,
        attachment: Option<AttachmentToken>,
    ) -> Result<(), String> {
        let mut first_error = None;
        if let Some(attachment) = attachment {
            if let Err(error) = drop_window(runtime, attachment) {
                remember_first_error(&mut first_error, error);
            }
        }
        let (hotkeys, activation, catalog_executor, request_event_loop_quit) = {
            let mut runtime_ref = runtime.borrow_mut();
            let request = !runtime_ref.quit_event_loop_requested;
            runtime_ref.quit_event_loop_requested = true;
            (
                runtime_ref.hotkeys.take(),
                runtime_ref.activation.take(),
                runtime_ref.catalog_executor.take(),
                request,
            )
        };
        if let Some(executor) = catalog_executor {
            if let Err(error) = executor.shutdown() {
                remember_first_error(&mut first_error, error);
            }
        }
        if let Some(hotkeys) = hotkeys {
            if let Err(error) = hotkeys.shutdown() {
                remember_first_error(&mut first_error, error.to_string());
            }
            runtime.borrow_mut().resources.hotkey_service_alive = false;
        }
        if let Some(activation) = activation {
            if let Err(error) = activation.shutdown() {
                remember_first_error(&mut first_error, error.to_string());
            }
            runtime.borrow_mut().resources.activation_service_alive = false;
        }
        {
            let mut runtime_ref = runtime.borrow_mut();
            runtime_ref.resources.tray_ready = false;
            runtime_ref.resources.desktop_consumer_alive = runtime_ref.desktop.consumer_alive();
        }
        if request_event_loop_quit {
            if let Err(error) = slint::quit_event_loop() {
                remember_first_error(&mut first_error, error.to_string());
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(())
    }

    fn shutdown_after_event_loop(runtime: &Rc<RefCell<SlintShell>>) -> Result<(), String> {
        if !runtime.borrow().lifecycle.snapshot().quitting {
            let action = runtime
                .borrow_mut()
                .lifecycle
                .handle_command(ShellCommand::Quit)
                .map_err(|error| error.to_string())?;
            execute_action(runtime, action)?;
        }
        Ok(())
    }

    fn with_controller(
        runtime: &Weak<RefCell<SlintShell>>,
        attachment: AttachmentToken,
        command: impl FnOnce(
            &PlaybackController<SessionCommandPort>,
        ) -> Result<(), crate::controller::ControllerError>,
    ) {
        let Some(runtime) = runtime.upgrade() else {
            return;
        };
        let mut runtime_ref = runtime.borrow_mut();
        if !runtime_ref
            .lifecycle
            .accept_callback(attachment)
            .unwrap_or(false)
        {
            return;
        }
        let Some(resources) = runtime_ref.window.as_ref() else {
            return;
        };
        let result = resources.session.as_ref().and_then(|session| {
            session
                .controller()
                .lock()
                .ok()
                .map(|controller| command(&controller))
        });
        if let Some(Err(error)) = result {
            resources
                .window
                .set_status_text(format!("Controller error: {error}").into());
        }
    }

    fn publish_recorder_snapshot(desktop: &SlintDesktopAdapter) -> Result<(), String> {
        desktop
            .try_publish(UiEvent::Recorder {
                generation: Generation::new(1),
                event: RecorderEvent::Status {
                    recording: true,
                    waiting_for_game: false,
                    segments: 2,
                    buffered_s: 30.0,
                    buffered_mb: 24.0,
                    full_session: false,
                    encoder: "H.264".into(),
                    capture_backend: "windows_graphics_capture".into(),
                },
            })
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn publish_lifecycle(
        runtime: &mut SlintShell,
        mode: WindowLifecycleMode,
    ) -> Result<(), String> {
        runtime.lifecycle_revision = runtime
            .lifecycle_revision
            .checked_next()
            .map_err(|error| error.to_string())?;
        runtime
            .desktop
            .try_publish(UiEvent::WindowLifecycle {
                snapshot: WindowLifecycleSnapshot::new(runtime.lifecycle_revision, mode),
            })
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn clear_window_models(window: &CliplineSpike) {
        window.set_cpu_video_frame(slint::Image::default());
        window.set_library_items(slint::ModelRc::new(slint::VecModel::from(
            Vec::<LibraryItem>::new(),
        )));
        window.set_catalog_dialog(crate::CatalogDialogModel::default());
        window.set_catalog_dialog_audio_tracks(slint::ModelRc::new(slint::VecModel::from(Vec::<
            crate::CatalogAudioTrack,
        >::new(
        ))));
        window.set_timeline_markers(slint::ModelRc::new(slint::VecModel::from(Vec::<
            TimelineMarker,
        >::new())));
        window.set_desktop_uploads(slint::ModelRc::new(slint::VecModel::from(Vec::<
            DesktopUploadItem,
        >::new())));
    }

    fn report_shell_error(runtime: &Rc<RefCell<SlintShell>>, error: &str) {
        let mut runtime_ref = runtime.borrow_mut();
        if let Some(resources) = runtime_ref.window.as_ref() {
            resources
                .window
                .set_status_text(format!("Native shell failed: {error}").into());
        }
        write_shell_marker(&mut runtime_ref, "error", error);
    }

    fn write_shell_marker(runtime: &mut SlintShell, kind: &str, detail: &str) {
        let Some(path) = runtime.options.marker_path.as_ref() else {
            return;
        };
        let Some(revision) = runtime.marker_revision.checked_add(1) else {
            return;
        };
        runtime.marker_revision = revision;
        let lifecycle = runtime.lifecycle.snapshot();
        let resources = runtime.resources;
        let value = serde_json::json!({
            "revision": revision,
            "trayReady": resources.tray_ready,
            "openAccepted": lifecycle.counters.open_requests,
            "closeAccepted": lifecycle.counters.close_requests,
            "windowCreated": lifecycle.counters.windows_created,
            "windowDropped": lifecycle.counters.windows_dropped,
            "liveWindows": u64::from(runtime.window.is_some()),
            "maxLiveWindows": resources.max_live_windows,
            "desktopAttached": resources.desktop_attached,
            "desktopDetached": resources.desktop_detached,
            "liveDesktopAttachments": resources.live_desktop_attachments,
            "playbackStarted": resources.playback_started,
            "playbackStopped": resources.playback_stopped,
            "livePlaybackSessions": resources.live_playback_sessions,
            "videoHostCreated": resources.video_hosts_created,
            "videoHostDropped": resources.video_hosts_dropped,
            "liveVideoHosts": resources.live_video_hosts,
            "modelSetsCreated": resources.model_sets_created,
            "modelSetsDropped": resources.model_sets_dropped,
            "liveModelSets": resources.live_model_sets,
            "presentationResourcesLive": resources.live_playback_sessions
                + resources.live_video_hosts
                + resources.live_model_sets,
            "desktopConsumerAlive": runtime.desktop.consumer_alive(),
            "hotkeyServiceAlive": runtime.hotkeys.is_some(),
            "activationServiceAlive": runtime.activation.is_some(),
            "staleClosuresRejected": lifecycle.counters.stale_callbacks,
            "quitAccepted": lifecycle.counters.quit_effects,
        });
        let _ = write_lifecycle_marker(path, kind, detail, &value);
    }

    fn checked_increment(counter: &mut u64) -> Result<(), String> {
        *counter = counter
            .checked_add(1)
            .ok_or_else(|| "Slint shell resource counter exhausted".to_owned())?;
        Ok(())
    }

    fn remember_first_error(first_error: &mut Option<String>, error: String) {
        if first_error.is_none() {
            *first_error = Some(error);
        }
    }

    fn tray_icon() -> slint::Image {
        let mut pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(16, 16);
        for (index, pixel) in pixels.make_mut_slice().iter_mut().enumerate() {
            let x = index % 16;
            let y = index / 16;
            *pixel = if (x as isize - 8).pow(2) + (y as isize - 8).pow(2) <= 49 {
                slint::Rgba8Pixel::new(217, 150, 42, 255)
            } else {
                slint::Rgba8Pixel::new(0, 0, 0, 0)
            };
        }
        slint::Image::from_rgba8(pixels)
    }
}

#[cfg(windows)]
pub use windows_runtime::{run as run_windows_shell, ShellResourceSnapshot, ShellRunReport};
