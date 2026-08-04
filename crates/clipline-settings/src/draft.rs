//! Framework-neutral Settings draft state and navigation.
//!
//! The controller owns only UI-editable preferences plus bounded, display-only
//! backend metadata. It intentionally cannot carry credentials, upload records,
//! or the complete persisted settings document.

use std::fmt;

use crate::SettingsPreferences;

const MAX_DISPLAY_TEXT_BYTES: usize = 4 * 1024;
const MAX_DISPLAY_UPLOADS: u32 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum SettingsTab {
    General,
    Capture,
    Recording,
    Storage,
    Hotkeys,
    Games,
    Cloud,
    Support,
}

impl SettingsTab {
    pub const ALL: [Self; 8] = [
        Self::General,
        Self::Capture,
        Self::Recording,
        Self::Storage,
        Self::Hotkeys,
        Self::Games,
        Self::Cloud,
        Self::Support,
    ];

    const fn index(self) -> usize {
        self as usize
    }

    const fn from_index(index: usize) -> Self {
        Self::ALL[index]
    }

    pub const fn next(self) -> Self {
        Self::from_index((self.index() + 1) % Self::ALL.len())
    }

    pub const fn previous(self) -> Self {
        Self::from_index((self.index() + Self::ALL.len() - 1) % Self::ALL.len())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabNavigation {
    Previous,
    Next,
    Home,
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabProjection {
    pub tab: SettingsTab,
    pub active: bool,
    pub focused: bool,
    pub tab_index: i8,
    pub dirty: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SettingsField {
    OpenOnStartup,
    CloseToTray,
    MinimizeToTray,
    LegacyTimelineEditor,
    UiTheme,
    UpdateChannel,
    CaptureMode,
    CaptureBackend,
    WindowTitle,
    CaptureRegion,
    AudioOutput,
    AudioMicrophone,
    ReplayWindow,
    VideoQuality,
    Bitrate,
    FramesPerSecond,
    AdvancedRecording,
    VideoEncoder,
    OutputResolution,
    MediaDirectory,
    MediaQuota,
    ReplayStorage,
    PrimaryHotkey,
    SecondaryHotkey,
    GamesAutoDetect,
    GamesPauseWhenNoGame,
    GamePlugins,
    CustomGames,
    CloudDefaultVisibility,
    CloudDeleteLocal,
    CloudAutoUpload,
}

impl SettingsField {
    pub const ALL: [Self; 31] = [
        Self::OpenOnStartup,
        Self::CloseToTray,
        Self::MinimizeToTray,
        Self::LegacyTimelineEditor,
        Self::UiTheme,
        Self::UpdateChannel,
        Self::CaptureMode,
        Self::CaptureBackend,
        Self::WindowTitle,
        Self::CaptureRegion,
        Self::AudioOutput,
        Self::AudioMicrophone,
        Self::ReplayWindow,
        Self::VideoQuality,
        Self::Bitrate,
        Self::FramesPerSecond,
        Self::AdvancedRecording,
        Self::VideoEncoder,
        Self::OutputResolution,
        Self::MediaDirectory,
        Self::MediaQuota,
        Self::ReplayStorage,
        Self::PrimaryHotkey,
        Self::SecondaryHotkey,
        Self::GamesAutoDetect,
        Self::GamesPauseWhenNoGame,
        Self::GamePlugins,
        Self::CustomGames,
        Self::CloudDefaultVisibility,
        Self::CloudDeleteLocal,
        Self::CloudAutoUpload,
    ];

    pub const fn tab(self) -> SettingsTab {
        match self {
            Self::OpenOnStartup
            | Self::CloseToTray
            | Self::MinimizeToTray
            | Self::LegacyTimelineEditor
            | Self::UiTheme
            | Self::UpdateChannel => SettingsTab::General,
            Self::CaptureMode
            | Self::CaptureBackend
            | Self::WindowTitle
            | Self::CaptureRegion
            | Self::AudioOutput
            | Self::AudioMicrophone => SettingsTab::Capture,
            Self::ReplayWindow
            | Self::VideoQuality
            | Self::Bitrate
            | Self::FramesPerSecond
            | Self::AdvancedRecording
            | Self::VideoEncoder
            | Self::OutputResolution => SettingsTab::Recording,
            Self::MediaDirectory | Self::MediaQuota | Self::ReplayStorage => SettingsTab::Storage,
            Self::PrimaryHotkey | Self::SecondaryHotkey => SettingsTab::Hotkeys,
            Self::GamesAutoDetect
            | Self::GamesPauseWhenNoGame
            | Self::GamePlugins
            | Self::CustomGames => SettingsTab::Games,
            Self::CloudDefaultVisibility | Self::CloudDeleteLocal | Self::CloudAutoUpload => {
                SettingsTab::Cloud
            }
        }
    }

    fn differs(self, left: &SettingsPreferences, right: &SettingsPreferences) -> bool {
        match self {
            Self::OpenOnStartup => left.open_on_startup != right.open_on_startup,
            Self::CloseToTray => left.close_to_tray != right.close_to_tray,
            Self::MinimizeToTray => left.minimize_to_tray != right.minimize_to_tray,
            Self::LegacyTimelineEditor => {
                left.legacy_timeline_editor != right.legacy_timeline_editor
            }
            Self::UiTheme => left.ui_theme != right.ui_theme,
            Self::UpdateChannel => left.update_channel != right.update_channel,
            Self::CaptureMode => left.capture_mode != right.capture_mode,
            Self::CaptureBackend => left.capture_backend != right.capture_backend,
            Self::WindowTitle => left.window_title != right.window_title,
            Self::CaptureRegion => left.capture_region != right.capture_region,
            Self::AudioOutput => {
                left.audio.output_enabled != right.audio.output_enabled
                    || left.audio.output_device_id != right.audio.output_device_id
                    || left.audio.output_volume != right.audio.output_volume
                    || left.audio.split_output_by_process != right.audio.split_output_by_process
            }
            Self::AudioMicrophone => {
                left.audio.mic_enabled != right.audio.mic_enabled
                    || left.audio.mic_device_id != right.audio.mic_device_id
                    || left.audio.mic_volume != right.audio.mic_volume
                    || left.audio.mic_channels != right.audio.mic_channels
            }
            Self::ReplayWindow => left.replay_window_s != right.replay_window_s,
            Self::VideoQuality => left.video_quality != right.video_quality,
            Self::Bitrate => left.bitrate_mbps != right.bitrate_mbps,
            Self::FramesPerSecond => left.fps != right.fps,
            Self::AdvancedRecording => left.advanced_recording != right.advanced_recording,
            Self::VideoEncoder => left.video_encoder != right.video_encoder,
            Self::OutputResolution => left.output_resolution != right.output_resolution,
            Self::MediaDirectory => left.media_dir != right.media_dir,
            Self::MediaQuota => left.disk_quota_gb != right.disk_quota_gb,
            Self::ReplayStorage => left.replay_storage != right.replay_storage,
            Self::PrimaryHotkey => left.hotkey != right.hotkey,
            Self::SecondaryHotkey => left.hotkey_secondary != right.hotkey_secondary,
            Self::GamesAutoDetect => left.games.auto_detect != right.games.auto_detect,
            Self::GamesPauseWhenNoGame => {
                left.games.pause_when_no_game != right.games.pause_when_no_game
            }
            Self::GamePlugins => left.games.plugins != right.games.plugins,
            Self::CustomGames => left.games.custom_games != right.games.custom_games,
            Self::CloudDefaultVisibility => {
                left.cloud.default_visibility != right.cloud.default_visibility
            }
            Self::CloudDeleteLocal => {
                left.cloud.delete_local_after_upload != right.cloud.delete_local_after_upload
            }
            Self::CloudAutoUpload => left.cloud.auto_upload_rules != right.cloud.auto_upload_rules,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirtySummary {
    pub fields: Vec<SettingsField>,
    pub tabs: Vec<SettingsTab>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SettingsSessionGeneration(u64);

impl SettingsSessionGeneration {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingsSaveToken {
    session: SettingsSessionGeneration,
    request_generation: u64,
    draft_revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseRequest {
    Explicit,
    Escape,
    Backdrop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseResult {
    CloseClean,
    WarningArmed,
    DiscardedAndClose,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DraftError {
    GenerationExhausted,
    StaleResult,
    SavedPreferencesMismatch,
    InvalidPreferences(String),
    InvalidDisplayState(String),
    AllocationFailed(&'static str),
}

impl fmt::Display for DraftError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GenerationExhausted => formatter.write_str("settings generation exhausted"),
            Self::StaleResult => formatter.write_str("stale settings result"),
            Self::SavedPreferencesMismatch => {
                formatter.write_str("saved preferences do not match the submitted draft")
            }
            Self::InvalidPreferences(error) => write!(formatter, "invalid preferences: {error}"),
            Self::InvalidDisplayState(error) => write!(formatter, "invalid display state: {error}"),
            Self::AllocationFailed(context) => {
                write!(formatter, "allocation failed while building {context}")
            }
        }
    }
}

impl std::error::Error for DraftError {}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CloudAccountOwner {
    pub stable_key: String,
    pub generation: u64,
}

/// Ownership for disconnected Cloud configuration work.
///
/// Account generations cannot fence a connect dialog because no account exists
/// yet. This token instead binds that work to the exact settings session and
/// Cloud configuration generation. Installing, replacing, or removing an
/// account advances the generation before any old work can publish.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CloudConfigurationOwner {
    pub session: SettingsSessionGeneration,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CloudWorkOwner {
    Account(CloudAccountOwner),
    Configuration(CloudConfigurationOwner),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudAccountDisplay {
    owner: CloudAccountOwner,
    pub display_name: String,
}

impl CloudAccountDisplay {
    pub fn new(
        stable_key: impl Into<String>,
        generation: u64,
        display_name: impl Into<String>,
    ) -> Result<Self, DraftError> {
        let stable_key = stable_key.into();
        let display_name = display_name.into();
        validate_display_text("cloud account key", &stable_key, false)?;
        validate_display_text("cloud display name", &display_name, true)?;
        Ok(Self {
            owner: CloudAccountOwner {
                stable_key,
                generation,
            },
            display_name,
        })
    }

    pub const fn owner(&self) -> &CloudAccountOwner {
        &self.owner
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettingsBackendDisplay {
    pub cloud_account: Option<CloudAccountDisplay>,
    pub upload_count: u32,
    pub osu_connected: bool,
}

impl SettingsBackendDisplay {
    fn validate(&self) -> Result<(), DraftError> {
        if self.upload_count > MAX_DISPLAY_UPLOADS {
            return Err(DraftError::InvalidDisplayState(format!(
                "upload count {} exceeds {MAX_DISPLAY_UPLOADS}",
                self.upload_count
            )));
        }
        if let Some(account) = &self.cloud_account {
            validate_display_text("cloud account key", &account.owner.stable_key, false)?;
            validate_display_text("cloud display name", &account.display_name, true)?;
        }
        Ok(())
    }

    fn owner(&self) -> Option<&CloudAccountOwner> {
        self.cloud_account.as_ref().map(|account| &account.owner)
    }
}

fn validate_display_text(label: &str, value: &str, empty_allowed: bool) -> Result<(), DraftError> {
    if (!empty_allowed && value.is_empty()) || value.len() > MAX_DISPLAY_TEXT_BYTES {
        return Err(DraftError::InvalidDisplayState(format!(
            "{label} must be {}..={MAX_DISPLAY_TEXT_BYTES} UTF-8 bytes",
            usize::from(!empty_allowed)
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudWorkKind {
    ConnectDialog,
    DisconnectDialog,
    Probe,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedCloudWork {
    pub session: SettingsSessionGeneration,
    pub owner: CloudWorkOwner,
    pub kind: CloudWorkKind,
    pub request_generation: u64,
}

#[derive(Debug, PartialEq)]
pub struct SettingsDraftController {
    session: SettingsSessionGeneration,
    request_generation: u64,
    draft_revision: u64,
    active_save: Option<SettingsSaveToken>,
    active_tab: SettingsTab,
    focused_tab: SettingsTab,
    baseline: SettingsPreferences,
    draft: SettingsPreferences,
    discard_warning_armed: bool,
    backend_display: SettingsBackendDisplay,
    cloud_configuration_generation: u64,
    owned_cloud_work: Option<OwnedCloudWork>,
}

impl SettingsDraftController {
    pub fn open(
        previous_session: Option<SettingsSessionGeneration>,
        preferences: SettingsPreferences,
    ) -> Result<Self, DraftError> {
        Self::open_with_request_generation(previous_session, 0, preferences)
    }

    pub fn open_with_request_generation(
        previous_session: Option<SettingsSessionGeneration>,
        request_generation: u64,
        preferences: SettingsPreferences,
    ) -> Result<Self, DraftError> {
        let session = previous_session.map_or(Ok(1), |generation| {
            generation
                .get()
                .checked_add(1)
                .ok_or(DraftError::GenerationExhausted)
        })?;
        let preferences = normalize(preferences)?;
        let baseline = preferences
            .try_clone_bounded()
            .map_err(DraftError::InvalidPreferences)?;
        Ok(Self {
            session: SettingsSessionGeneration::new(session),
            request_generation,
            draft_revision: 0,
            active_save: None,
            active_tab: SettingsTab::General,
            focused_tab: SettingsTab::General,
            baseline,
            draft: preferences,
            discard_warning_armed: false,
            backend_display: SettingsBackendDisplay::default(),
            cloud_configuration_generation: 1,
            owned_cloud_work: None,
        })
    }

    pub const fn session(&self) -> SettingsSessionGeneration {
        self.session
    }

    pub const fn active_tab(&self) -> SettingsTab {
        self.active_tab
    }

    pub fn baseline(&self) -> &SettingsPreferences {
        &self.baseline
    }

    pub fn draft(&self) -> &SettingsPreferences {
        &self.draft
    }

    pub const fn discard_warning_armed(&self) -> bool {
        self.discard_warning_armed
    }

    pub fn backend_display(&self) -> &SettingsBackendDisplay {
        &self.backend_display
    }

    pub fn owned_cloud_work(&self) -> Option<&OwnedCloudWork> {
        self.owned_cloud_work.as_ref()
    }

    pub const fn cloud_configuration_owner(&self) -> CloudConfigurationOwner {
        CloudConfigurationOwner {
            session: self.session,
            generation: self.cloud_configuration_generation,
        }
    }

    pub fn activate_tab(&mut self, tab: SettingsTab) {
        self.active_tab = tab;
        self.focused_tab = tab;
        self.discard_warning_armed = false;
    }

    pub fn navigate(&mut self, navigation: TabNavigation) {
        let next = match navigation {
            TabNavigation::Previous => self.focused_tab.previous(),
            TabNavigation::Next => self.focused_tab.next(),
            TabNavigation::Home => SettingsTab::ALL[0],
            TabNavigation::End => SettingsTab::ALL[SettingsTab::ALL.len() - 1],
        };
        self.activate_tab(next);
    }

    pub fn tab_projection(&self) -> Result<Vec<TabProjection>, DraftError> {
        let mut projection = Vec::new();
        projection
            .try_reserve_exact(SettingsTab::ALL.len())
            .map_err(|_| DraftError::AllocationFailed("settings tab projection"))?;
        projection.extend(SettingsTab::ALL.into_iter().map(|tab| TabProjection {
            tab,
            active: tab == self.active_tab,
            focused: tab == self.focused_tab,
            tab_index: if tab == self.focused_tab { 0 } else { -1 },
            dirty: self.is_tab_dirty(tab),
        }));
        Ok(projection)
    }

    pub fn replace_draft(&mut self, replacement: SettingsPreferences) -> Result<(), DraftError> {
        let replacement = normalize(replacement)?;
        if replacement == self.draft {
            self.discard_warning_armed = false;
            return Ok(());
        }
        let next_revision = self
            .draft_revision
            .checked_add(1)
            .ok_or(DraftError::GenerationExhausted)?;
        self.draft = replacement;
        self.draft_revision = next_revision;
        self.active_save = None;
        self.discard_warning_armed = false;
        Ok(())
    }

    pub fn is_dirty(&self) -> bool {
        self.draft != self.baseline
    }

    pub fn is_field_dirty(&self, field: SettingsField) -> bool {
        field.differs(&self.baseline, &self.draft)
    }

    pub fn is_tab_dirty(&self, tab: SettingsTab) -> bool {
        SettingsField::ALL
            .into_iter()
            .any(|field| field.tab() == tab && self.is_field_dirty(field))
    }

    pub fn dirty_summary(&self) -> Result<DirtySummary, DraftError> {
        let field_count = SettingsField::ALL
            .into_iter()
            .filter(|field| self.is_field_dirty(*field))
            .count();
        let mut fields = Vec::new();
        fields
            .try_reserve_exact(field_count)
            .map_err(|_| DraftError::AllocationFailed("dirty settings fields"))?;
        fields.extend(
            SettingsField::ALL
                .into_iter()
                .filter(|field| self.is_field_dirty(*field)),
        );

        let tab_count = SettingsTab::ALL
            .into_iter()
            .filter(|tab| self.is_tab_dirty(*tab))
            .count();
        let mut tabs = Vec::new();
        tabs.try_reserve_exact(tab_count)
            .map_err(|_| DraftError::AllocationFailed("dirty settings tabs"))?;
        tabs.extend(
            SettingsTab::ALL
                .into_iter()
                .filter(|tab| self.is_tab_dirty(*tab)),
        );
        Ok(DirtySummary { fields, tabs })
    }

    pub fn save_visible(&self) -> bool {
        self.active_tab != SettingsTab::Support || self.is_dirty()
    }

    pub fn begin_save(&mut self) -> Result<SettingsSaveToken, DraftError> {
        let request_generation = self
            .request_generation
            .checked_add(1)
            .ok_or(DraftError::GenerationExhausted)?;
        let token = SettingsSaveToken {
            session: self.session,
            request_generation,
            draft_revision: self.draft_revision,
        };
        self.request_generation = request_generation;
        self.active_save = Some(token);
        Ok(token)
    }

    pub fn accept_saved(
        &mut self,
        token: SettingsSaveToken,
        persisted: SettingsPreferences,
    ) -> Result<(), DraftError> {
        if self.active_save != Some(token)
            || token.session != self.session
            || token.draft_revision != self.draft_revision
        {
            return Err(DraftError::StaleResult);
        }
        let persisted = normalize(persisted)?;
        if persisted != self.draft {
            return Err(DraftError::SavedPreferencesMismatch);
        }
        let next_revision = self
            .draft_revision
            .checked_add(1)
            .ok_or(DraftError::GenerationExhausted)?;
        let baseline = persisted
            .try_clone_bounded()
            .map_err(DraftError::InvalidPreferences)?;
        self.baseline = baseline;
        self.draft = persisted;
        self.draft_revision = next_revision;
        self.active_save = None;
        self.discard_warning_armed = false;
        Ok(())
    }

    pub fn discard(&mut self) -> Result<(), DraftError> {
        if self.draft == self.baseline {
            self.discard_warning_armed = false;
            self.active_save = None;
            return Ok(());
        }
        let next_revision = self
            .draft_revision
            .checked_add(1)
            .ok_or(DraftError::GenerationExhausted)?;
        let replacement = self
            .baseline
            .try_clone_bounded()
            .map_err(DraftError::InvalidPreferences)?;
        self.draft = replacement;
        self.draft_revision = next_revision;
        self.active_save = None;
        self.discard_warning_armed = false;
        Ok(())
    }

    pub fn request_close(&mut self, request: CloseRequest) -> Result<CloseResult, DraftError> {
        if !self.is_dirty() {
            self.discard_warning_armed = false;
            return Ok(CloseResult::CloseClean);
        }
        if request == CloseRequest::Backdrop || !self.discard_warning_armed {
            self.discard_warning_armed = true;
            return Ok(CloseResult::WarningArmed);
        }
        self.discard()?;
        Ok(CloseResult::DiscardedAndClose)
    }

    pub fn reconcile_backend(
        &mut self,
        replacement: SettingsBackendDisplay,
    ) -> Result<(), DraftError> {
        replacement.validate()?;
        let owner_changed = self.backend_display.owner() != replacement.owner();
        let next_configuration_generation = if owner_changed {
            self.cloud_configuration_generation
                .checked_add(1)
                .ok_or(DraftError::GenerationExhausted)?
        } else {
            self.cloud_configuration_generation
        };
        if owner_changed {
            self.owned_cloud_work = None;
        }
        self.cloud_configuration_generation = next_configuration_generation;
        self.backend_display = replacement;
        Ok(())
    }

    pub fn own_cloud_work(
        &mut self,
        kind: CloudWorkKind,
    ) -> Result<Option<OwnedCloudWork>, DraftError> {
        let request_generation = self
            .request_generation
            .checked_add(1)
            .ok_or(DraftError::GenerationExhausted)?;
        let owner = match (kind, self.backend_display.owner()) {
            (CloudWorkKind::ConnectDialog, _) | (CloudWorkKind::Probe, None) => {
                CloudWorkOwner::Configuration(self.cloud_configuration_owner())
            }
            (_, Some(owner)) => CloudWorkOwner::Account(try_clone_account_owner(owner)?),
            (CloudWorkKind::DisconnectDialog, None) => return Ok(None),
        };
        let stored_owner = try_clone_cloud_work_owner(&owner)?;
        let work = OwnedCloudWork {
            session: self.session,
            owner,
            kind,
            request_generation,
        };
        self.request_generation = request_generation;
        self.owned_cloud_work = Some(OwnedCloudWork {
            session: self.session,
            owner: stored_owner,
            kind,
            request_generation,
        });
        Ok(Some(work))
    }

    /// Accepts only the exact latest Cloud operation and consumes its owner.
    pub fn accept_cloud_work(&mut self, work: &OwnedCloudWork) -> Result<(), DraftError> {
        if self.owned_cloud_work.as_ref() != Some(work) {
            return Err(DraftError::StaleResult);
        }
        self.owned_cloud_work = None;
        Ok(())
    }
}

fn normalize(preferences: SettingsPreferences) -> Result<SettingsPreferences, DraftError> {
    preferences
        .normalized()
        .map_err(DraftError::InvalidPreferences)
}

fn try_clone_display_string(value: &str) -> Result<String, DraftError> {
    let mut clone = String::new();
    clone
        .try_reserve_exact(value.len())
        .map_err(|_| DraftError::AllocationFailed("cloud work owner"))?;
    clone.push_str(value);
    Ok(clone)
}

fn try_clone_account_owner(owner: &CloudAccountOwner) -> Result<CloudAccountOwner, DraftError> {
    Ok(CloudAccountOwner {
        stable_key: try_clone_display_string(&owner.stable_key)?,
        generation: owner.generation,
    })
}

fn try_clone_cloud_work_owner(owner: &CloudWorkOwner) -> Result<CloudWorkOwner, DraftError> {
    match owner {
        CloudWorkOwner::Account(owner) => {
            Ok(CloudWorkOwner::Account(try_clone_account_owner(owner)?))
        }
        CloudWorkOwner::Configuration(owner) => Ok(CloudWorkOwner::Configuration(*owner)),
    }
}
