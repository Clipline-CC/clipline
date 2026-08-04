//! Exact-owner Games catalog controller for the native Settings surface.

use std::array;
use std::error::Error;
use std::fmt;

use clipline_settings::{GamePreferences, ProbeKind, ProbeSessionOwner, ProbeToken};

use crate::channel::{GamesProbeCatalog, GamesProbeFailed, GamesProbeReady};
use crate::icon::{GameIconId, GameIconLoadState};
use crate::identity::GameItemIdentity;
use crate::plugin::GamePluginInfo;
use crate::presentation::{
    game_page_count, game_page_window, GameCandidateCatalog, GameCatalog, GameCatalogInput,
    GamePageIndex, GamePageOutcome, GamePageProjection, GamePageWindow, GamePresentationError,
    GameProjectionReservation, ResolvedGameCatalogMember,
};

pub const MAX_GAME_SELECTION: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GameCatalogRevision(u64);

impl GameCatalogRevision {
    const INITIAL: Self = Self(1);

    pub const fn get(self) -> u64 {
        self.0
    }

    fn checked_next(self) -> Result<Self, GamesControllerError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(GamesControllerError::RevisionExhausted)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GameViewGeneration(u64);

impl GameViewGeneration {
    const INITIAL: Self = Self(1);

    pub const fn get(self) -> u64 {
        self.0
    }

    fn checked_next(self) -> Result<Self, GamesControllerError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(GamesControllerError::ViewGenerationExhausted)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameCandidateSource {
    Installed,
    RunningWindows,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GamesProbePhase {
    Idle,
    Pending,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GamesProbeStatus {
    pub token: Option<ProbeToken>,
    pub phase: GamesProbePhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GamesActionFence {
    pub owner: ProbeSessionOwner,
    pub revision: GameCatalogRevision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GamesSummary {
    pub owner: ProbeSessionOwner,
    pub revision: GameCatalogRevision,
    pub view_generation: GameViewGeneration,
    pub attached: bool,
    pub catalog_ready: bool,
    pub candidate_source: GameCandidateSource,
    pub page: GamePageIndex,
    pub page_count: usize,
    pub total: usize,
    pub selected_count: usize,
    pub plugins: GamesProbeStatus,
    pub installed: GamesProbeStatus,
    pub running_windows: GamesProbeStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GamesControllerUpdate {
    pub summary: GamesSummary,
    pub page_corrected: bool,
}

pub struct RejectedGamesSettingsUpdate {
    pub error: GamesControllerError,
    pub settings: GamePreferences,
}

pub struct RejectedGamesProbeReady {
    pub error: GamesControllerError,
    pub token: ProbeToken,
    pub catalog: GamesProbeCatalog,
}

impl fmt::Debug for RejectedGamesProbeReady {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RejectedGamesProbeReady")
            .field("error", &self.error)
            .field("token", &self.token)
            .field("catalog", &self.catalog)
            .finish()
    }
}

impl fmt::Debug for RejectedGamesSettingsUpdate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RejectedGamesSettingsUpdate")
            .field("error", &self.error)
            .field("plugin_count", &self.settings.plugins.len())
            .field("custom_count", &self.settings.custom_games.len())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GamesControllerError {
    WrongOwner,
    WrongProbeKind,
    StaleProbe,
    StaleAction,
    UnexpectedProbeResult,
    Detached,
    CatalogUnavailable,
    InvalidSelection,
    PastEnd { fallback: Option<GamePageIndex> },
    RevisionExhausted,
    ViewGenerationExhausted,
    Presentation(GamePresentationError),
}

impl fmt::Display for GamesControllerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongOwner => formatter.write_str("Games work belongs to another Settings owner"),
            Self::WrongProbeKind => formatter.write_str("probe is not a Games catalog probe"),
            Self::StaleProbe => formatter.write_str("Games probe generation is stale"),
            Self::StaleAction => formatter.write_str("Games action fence is stale"),
            Self::UnexpectedProbeResult => {
                formatter.write_str("Games probe result was not the exact pending request")
            }
            Self::Detached => formatter.write_str("Games controller is detached"),
            Self::CatalogUnavailable => formatter.write_str("Games catalog is not ready"),
            Self::InvalidSelection => {
                formatter.write_str("Games selection is not bounded current candidate authority")
            }
            Self::PastEnd { .. } => formatter.write_str("requested Games page is past the end"),
            Self::RevisionExhausted => formatter.write_str("Games catalog revision exhausted"),
            Self::ViewGenerationExhausted => formatter.write_str("Games view generation exhausted"),
            Self::Presentation(error) => error.fmt(formatter),
        }
    }
}

impl Error for GamesControllerError {}

impl From<GamePresentationError> for GamesControllerError {
    fn from(error: GamePresentationError) -> Self {
        Self::Presentation(error)
    }
}

#[derive(Debug)]
struct ProbeLane {
    token: Option<ProbeToken>,
    phase: GamesProbePhase,
    failure: Option<String>,
}

impl ProbeLane {
    fn new() -> Self {
        Self {
            token: None,
            phase: GamesProbePhase::Idle,
            failure: None,
        }
    }

    const fn status(&self) -> GamesProbeStatus {
        GamesProbeStatus {
            token: self.token,
            phase: self.phase,
        }
    }
}

pub struct GamesController {
    owner: ProbeSessionOwner,
    revision: GameCatalogRevision,
    view_generation: GameViewGeneration,
    lanes: [ProbeLane; 3],
    catalog: Option<GameCatalog>,
    pending_settings: Option<GamePreferences>,
    installed: Option<GameCandidateCatalog>,
    running_windows: Option<GameCandidateCatalog>,
    candidate_source: GameCandidateSource,
    page: GamePageIndex,
    selected: Vec<GameItemIdentity>,
    detached: bool,
}

impl GamesController {
    pub fn new(owner: ProbeSessionOwner, settings: GamePreferences) -> Self {
        Self {
            owner,
            revision: GameCatalogRevision::INITIAL,
            view_generation: GameViewGeneration::INITIAL,
            lanes: array::from_fn(|_| ProbeLane::new()),
            catalog: None,
            pending_settings: Some(settings),
            installed: None,
            running_windows: None,
            candidate_source: GameCandidateSource::Installed,
            page: GamePageIndex::new(0),
            selected: Vec::new(),
            detached: false,
        }
    }

    pub const fn owner(&self) -> ProbeSessionOwner {
        self.owner
    }

    pub const fn action_fence(&self) -> GamesActionFence {
        GamesActionFence {
            owner: self.owner,
            revision: self.revision,
        }
    }

    pub fn summary(&self) -> GamesSummary {
        let total = self.catalog.as_ref().map_or(0, GameCatalog::len);
        GamesSummary {
            owner: self.owner,
            revision: self.revision,
            view_generation: self.view_generation,
            attached: !self.detached,
            catalog_ready: self.catalog.is_some(),
            candidate_source: self.candidate_source,
            page: self.page,
            page_count: game_page_count(total),
            total,
            selected_count: self.selected.len(),
            plugins: self.lanes[0].status(),
            installed: self.lanes[1].status(),
            running_windows: self.lanes[2].status(),
        }
    }

    pub fn probe_failure(&self, kind: ProbeKind) -> Result<Option<&str>, GamesControllerError> {
        self.require_attached()?;
        Ok(self.lanes[probe_lane_index(kind)?].failure.as_deref())
    }

    pub fn register_probe(
        &mut self,
        token: ProbeToken,
    ) -> Result<GamesSummary, GamesControllerError> {
        self.require_attached()?;
        if token.owner != self.owner {
            return Err(GamesControllerError::WrongOwner);
        }
        if token.request_generation.get() == 0 {
            return Err(GamesControllerError::StaleProbe);
        }
        let next_view_generation = self.view_generation.checked_next()?;
        let lane = &mut self.lanes[probe_lane_index(token.kind)?];
        if lane
            .token
            .is_some_and(|current| token.request_generation <= current.request_generation)
        {
            return Err(GamesControllerError::StaleProbe);
        }
        lane.token = Some(token);
        lane.phase = GamesProbePhase::Pending;
        lane.failure = None;
        self.view_generation = next_view_generation;
        Ok(self.summary())
    }

    pub fn accept_probe_ready(
        &mut self,
        ready: GamesProbeReady,
        reservation: &dyn GameProjectionReservation,
    ) -> Result<GamesControllerUpdate, Box<RejectedGamesProbeReady>> {
        let token = ready.token();
        let catalog = ready.into_catalog();
        if let Err(error) = self
            .require_attached()
            .and_then(|()| self.require_pending(token))
        {
            return Err(Box::new(RejectedGamesProbeReady {
                error,
                token,
                catalog,
            }));
        }
        let lane_index = match probe_lane_index(token.kind) {
            Ok(index) => index,
            Err(error) => {
                return Err(Box::new(RejectedGamesProbeReady {
                    error,
                    token,
                    catalog,
                }))
            }
        };
        let next_revision = match self.revision.checked_next() {
            Ok(revision) => revision,
            Err(error) => {
                return Err(Box::new(RejectedGamesProbeReady {
                    error,
                    token,
                    catalog,
                }))
            }
        };
        let next_view_generation = match self.view_generation.checked_next() {
            Ok(generation) => generation,
            Err(error) => {
                return Err(Box::new(RejectedGamesProbeReady {
                    error,
                    token,
                    catalog,
                }))
            }
        };
        let result = match catalog {
            GamesProbeCatalog::Plugins(plugins) => self.accept_plugins(token, plugins, reservation),
            GamesProbeCatalog::Installed(catalog) => self.accept_candidates(
                GameCandidateSource::Installed,
                GameCandidateCatalog::Installed(catalog),
                reservation,
            ),
            GamesProbeCatalog::RunningWindows(catalog) => self.accept_candidates(
                GameCandidateSource::RunningWindows,
                GameCandidateCatalog::RunningWindows(catalog),
                reservation,
            ),
        };
        if let Err((error, catalog)) = result {
            return Err(Box::new(RejectedGamesProbeReady {
                error,
                token,
                catalog,
            }));
        }
        let lane = &mut self.lanes[lane_index];
        lane.phase = GamesProbePhase::Ready;
        lane.failure = None;
        self.revision = next_revision;
        self.view_generation = next_view_generation;
        let page_corrected = self.normalize_catalog_state();
        Ok(GamesControllerUpdate {
            summary: self.summary(),
            page_corrected,
        })
    }

    pub fn accept_probe_failed(
        &mut self,
        failed: GamesProbeFailed,
    ) -> Result<GamesControllerUpdate, GamesControllerError> {
        self.require_attached()?;
        let token = failed.token();
        self.require_pending(token)?;
        let next_view_generation = self.view_generation.checked_next()?;
        let (token, error) = failed.into_parts();
        let lane = &mut self.lanes[probe_lane_index(token.kind)?];
        lane.phase = GamesProbePhase::Failed;
        lane.failure = Some(error);
        self.view_generation = next_view_generation;
        Ok(GamesControllerUpdate {
            summary: self.summary(),
            page_corrected: false,
        })
    }

    pub fn replace_settings(
        &mut self,
        fence: GamesActionFence,
        settings: GamePreferences,
        reservation: &dyn GameProjectionReservation,
    ) -> Result<GamesControllerUpdate, Box<RejectedGamesSettingsUpdate>> {
        if let Err(error) = self.require_fence(fence) {
            return Err(Box::new(RejectedGamesSettingsUpdate { error, settings }));
        }
        let next_revision = match self.revision.checked_next() {
            Ok(revision) => revision,
            Err(error) => {
                return Err(Box::new(RejectedGamesSettingsUpdate { error, settings }));
            }
        };
        let next_view_generation = match self.view_generation.checked_next() {
            Ok(generation) => generation,
            Err(error) => {
                return Err(Box::new(RejectedGamesSettingsUpdate { error, settings }));
            }
        };
        if let Some(catalog) = self.catalog.as_mut() {
            if let Err(rejected) = catalog.try_replace_settings_recoverable(settings, reservation) {
                return Err(Box::new(RejectedGamesSettingsUpdate {
                    error: GamesControllerError::Presentation(rejected.error),
                    settings: rejected.settings,
                }));
            }
        } else {
            self.pending_settings = Some(settings);
        }
        self.revision = next_revision;
        self.view_generation = next_view_generation;
        let page_corrected = self.normalize_catalog_state();
        Ok(GamesControllerUpdate {
            summary: self.summary(),
            page_corrected,
        })
    }

    pub fn set_candidate_source(
        &mut self,
        fence: GamesActionFence,
        source: GameCandidateSource,
        reservation: &dyn GameProjectionReservation,
    ) -> Result<GamesControllerUpdate, GamesControllerError> {
        self.require_fence(fence)?;
        if source == self.candidate_source {
            return Ok(GamesControllerUpdate {
                summary: self.summary(),
                page_corrected: false,
            });
        }
        let next_revision = self.revision.checked_next()?;
        let next_view_generation = self.view_generation.checked_next()?;
        if self.catalog.is_some() {
            let replacement = self.take_candidate(source);
            let replacement_result = match self.catalog.as_mut() {
                Some(catalog) => {
                    catalog.try_replace_candidates_recoverable(replacement, reservation)
                }
                None => unreachable!("catalog presence checked before moving candidate authority"),
            };
            let replaced = match replacement_result {
                Ok(replaced) => replaced,
                Err(rejected) => {
                    self.put_candidate(source, rejected.candidates);
                    return Err(GamesControllerError::Presentation(rejected.error));
                }
            };
            self.put_candidate(self.candidate_source, replaced);
        }
        self.candidate_source = source;
        self.revision = next_revision;
        self.view_generation = next_view_generation;
        let page_corrected = self.normalize_catalog_state();
        Ok(GamesControllerUpdate {
            summary: self.summary(),
            page_corrected,
        })
    }

    pub fn replace_selection(
        &mut self,
        fence: GamesActionFence,
        selected: Vec<GameItemIdentity>,
    ) -> Result<GamesControllerUpdate, GamesControllerError> {
        self.require_fence(fence)?;
        let catalog = self
            .catalog
            .as_ref()
            .ok_or(GamesControllerError::CatalogUnavailable)?;
        validate_selection(catalog, &selected)?;
        let next_revision = self.revision.checked_next()?;
        let next_view_generation = self.view_generation.checked_next()?;
        self.selected = selected;
        self.revision = next_revision;
        self.view_generation = next_view_generation;
        Ok(GamesControllerUpdate {
            summary: self.summary(),
            page_corrected: false,
        })
    }

    pub fn set_page(
        &mut self,
        fence: GamesActionFence,
        page: GamePageIndex,
        reservation: &dyn GameProjectionReservation,
    ) -> Result<GamesControllerUpdate, GamesControllerError> {
        self.require_fence(fence)?;
        let catalog = self
            .catalog
            .as_ref()
            .ok_or(GamesControllerError::CatalogUnavailable)?;
        let next_view_generation = self.view_generation.checked_next()?;
        match catalog.project_page(
            page,
            &self.selected,
            |_| GameIconLoadState::Missing,
            reservation,
        )? {
            GamePageOutcome::Page(_) => self.page = page,
            GamePageOutcome::PastEnd { fallback_page, .. } => {
                return Err(GamesControllerError::PastEnd {
                    fallback: fallback_page,
                })
            }
        }
        self.view_generation = next_view_generation;
        Ok(GamesControllerUpdate {
            summary: self.summary(),
            page_corrected: false,
        })
    }

    pub fn project_current<F>(
        &self,
        icon_state: F,
        reservation: &dyn GameProjectionReservation,
    ) -> Result<GamePageProjection, GamesControllerError>
    where
        F: FnMut(&GameIconId) -> GameIconLoadState,
    {
        self.require_attached()?;
        let catalog = self
            .catalog
            .as_ref()
            .ok_or(GamesControllerError::CatalogUnavailable)?;
        match catalog.project_page(self.page, &self.selected, icon_state, reservation)? {
            GamePageOutcome::Page(page) => Ok(page),
            GamePageOutcome::PastEnd { fallback_page, .. } => Err(GamesControllerError::PastEnd {
                fallback: fallback_page,
            }),
        }
    }

    pub fn detach(
        &mut self,
        owner: ProbeSessionOwner,
    ) -> Result<GamesControllerUpdate, GamesControllerError> {
        self.require_attached()?;
        if owner != self.owner {
            return Err(GamesControllerError::WrongOwner);
        }
        let next_revision = self.revision.checked_next()?;
        let next_view_generation = self.view_generation.checked_next()?;
        self.catalog = None;
        self.pending_settings = None;
        self.installed = None;
        self.running_windows = None;
        self.selected.clear();
        self.lanes = array::from_fn(|_| ProbeLane::new());
        self.candidate_source = GameCandidateSource::Installed;
        self.page = GamePageIndex::new(0);
        self.revision = next_revision;
        self.view_generation = next_view_generation;
        self.detached = true;
        Ok(GamesControllerUpdate {
            summary: self.summary(),
            page_corrected: false,
        })
    }

    fn accept_plugins(
        &mut self,
        token: ProbeToken,
        plugins: Vec<GamePluginInfo>,
        reservation: &dyn GameProjectionReservation,
    ) -> Result<(), (GamesControllerError, GamesProbeCatalog)> {
        if let Some(catalog) = self.catalog.as_mut() {
            return match catalog.try_replace_plugins_recoverable(token, plugins, reservation) {
                Ok(_) => Ok(()),
                Err(rejected) => Err((
                    GamesControllerError::Presentation(rejected.error),
                    GamesProbeCatalog::Plugins(rejected.plugins),
                )),
            };
        }
        let Some(settings) = self.pending_settings.take() else {
            return Err((
                GamesControllerError::CatalogUnavailable,
                GamesProbeCatalog::Plugins(plugins),
            ));
        };
        let candidates = self.take_candidate(self.candidate_source);
        let input = GameCatalogInput {
            owner: self.owner,
            plugins_token: token,
            plugins,
            settings,
            candidates,
        };
        match GameCatalog::try_build_recoverable(input, reservation) {
            Ok(catalog) => {
                self.catalog = Some(catalog);
                Ok(())
            }
            Err(rejected) => {
                let input = rejected.input;
                self.pending_settings = Some(input.settings);
                self.put_candidate(self.candidate_source, input.candidates);
                Err((
                    GamesControllerError::Presentation(rejected.error),
                    GamesProbeCatalog::Plugins(input.plugins),
                ))
            }
        }
    }

    fn accept_candidates(
        &mut self,
        source: GameCandidateSource,
        candidates: GameCandidateCatalog,
        reservation: &dyn GameProjectionReservation,
    ) -> Result<(), (GamesControllerError, GamesProbeCatalog)> {
        if source == self.candidate_source {
            if let Some(catalog) = self.catalog.as_mut() {
                return match catalog
                    .try_replace_candidates_recoverable(Some(candidates), reservation)
                {
                    Ok(_) => Ok(()),
                    Err(rejected) => {
                        let Some(candidates) = rejected.candidates else {
                            unreachable!("Some candidate replacement must return owned authority")
                        };
                        Err((
                            GamesControllerError::Presentation(rejected.error),
                            candidate_probe_catalog(candidates),
                        ))
                    }
                };
            }
        }
        self.put_candidate(source, Some(candidates));
        Ok(())
    }

    fn normalize_catalog_state(&mut self) -> bool {
        let Some(catalog) = self.catalog.as_ref() else {
            self.page = GamePageIndex::new(0);
            self.selected.clear();
            return false;
        };
        self.selected.retain(|identity| {
            matches!(
                catalog.resolve(identity),
                Some(
                    ResolvedGameCatalogMember::InstalledCandidate(_)
                        | ResolvedGameCatalogMember::RunningWindow(_)
                )
            )
        });
        match game_page_window(catalog.len(), self.page) {
            GamePageWindow::Page { .. } => false,
            GamePageWindow::PastEnd { fallback_page, .. } => {
                self.page = fallback_page.unwrap_or(GamePageIndex::new(0));
                true
            }
        }
    }

    fn take_candidate(&mut self, source: GameCandidateSource) -> Option<GameCandidateCatalog> {
        match source {
            GameCandidateSource::Installed => self.installed.take(),
            GameCandidateSource::RunningWindows => self.running_windows.take(),
        }
    }

    fn put_candidate(
        &mut self,
        source: GameCandidateSource,
        candidates: Option<GameCandidateCatalog>,
    ) {
        match source {
            GameCandidateSource::Installed => self.installed = candidates,
            GameCandidateSource::RunningWindows => self.running_windows = candidates,
        }
    }

    fn require_attached(&self) -> Result<(), GamesControllerError> {
        if self.detached {
            Err(GamesControllerError::Detached)
        } else {
            Ok(())
        }
    }

    fn require_fence(&self, fence: GamesActionFence) -> Result<(), GamesControllerError> {
        self.require_attached()?;
        if fence.owner != self.owner {
            Err(GamesControllerError::WrongOwner)
        } else if fence.revision != self.revision {
            Err(GamesControllerError::StaleAction)
        } else {
            Ok(())
        }
    }

    fn require_pending(&self, token: ProbeToken) -> Result<(), GamesControllerError> {
        if token.owner != self.owner {
            return Err(GamesControllerError::WrongOwner);
        }
        let lane = &self.lanes[probe_lane_index(token.kind)?];
        if lane.token == Some(token) && lane.phase == GamesProbePhase::Pending {
            Ok(())
        } else {
            Err(GamesControllerError::UnexpectedProbeResult)
        }
    }
}

fn probe_lane_index(kind: ProbeKind) -> Result<usize, GamesControllerError> {
    match kind {
        ProbeKind::GamePlugins => Ok(0),
        ProbeKind::InstalledGames => Ok(1),
        ProbeKind::GameWindows => Ok(2),
        _ => Err(GamesControllerError::WrongProbeKind),
    }
}

fn candidate_probe_catalog(candidates: GameCandidateCatalog) -> GamesProbeCatalog {
    match candidates {
        GameCandidateCatalog::Installed(catalog) => GamesProbeCatalog::Installed(catalog),
        GameCandidateCatalog::RunningWindows(catalog) => GamesProbeCatalog::RunningWindows(catalog),
    }
}

fn validate_selection(
    catalog: &GameCatalog,
    selected: &[GameItemIdentity],
) -> Result<(), GamesControllerError> {
    if selected.len() > MAX_GAME_SELECTION
        || selected.windows(2).any(|pair| pair[0] >= pair[1])
        || selected.iter().any(|identity| {
            !matches!(identity, GameItemIdentity::Candidate(_))
                || !matches!(
                    catalog.resolve(identity),
                    Some(
                        ResolvedGameCatalogMember::InstalledCandidate(_)
                            | ResolvedGameCatalogMember::RunningWindow(_)
                    )
                )
        })
    {
        return Err(GamesControllerError::InvalidSelection);
    }
    Ok(())
}
