//! Pure, bounded Games catalog ownership and page projection.

use std::error::Error;
use std::fmt;

use clipline_settings::{
    CustomGameSettings, GamePreferences, GameRecordingMode, ProbeKind, ProbeSessionOwner,
    ProbeToken, MAX_SETTINGS_CUSTOM_GAMES, MAX_SETTINGS_FIELD_BYTES, MAX_SETTINGS_GAME_PLUGINS,
};

use crate::detection::GameWindowInfo;
use crate::discovery::{
    discovery_fields_match_custom_game, matches_existing_custom_game, DetectedGameCandidate,
};
use crate::icon::{GameIconId, GameIconLoadState};
use crate::identity::{
    CustomGameIdentity, GameItemIdentity, GameWindowIdentityCatalog, InstalledGameIdentityCatalog,
    PluginGameIdentity, MAX_CANDIDATE_CATALOG_ENTRIES,
};
use crate::plugin::GamePluginInfo;

pub const MAX_GAME_PAGE_ROWS: usize = 60;
pub const MAX_GAME_CATALOG_ROWS: usize =
    MAX_SETTINGS_GAME_PLUGINS + MAX_SETTINGS_CUSTOM_GAMES + MAX_CANDIDATE_CATALOG_ENTRIES;
pub const MAX_GAME_ROW_TEXT_BYTES: usize = MAX_SETTINGS_FIELD_BYTES;

const _: () = assert!(MAX_GAME_CATALOG_ROWS == 400);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GamePresentationError {
    WrongProbeKind {
        expected: ProbeKind,
        actual: ProbeKind,
    },
    OwnerMismatch,
    CatalogTooLarge {
        actual: usize,
        maximum: usize,
    },
    InvalidPlugin,
    PluginOrder,
    InvalidCustom,
    DuplicateIdentity,
    InvalidSelection,
    InvalidIconIdentity,
    Allocation {
        field: &'static str,
    },
}

impl fmt::Display for GamePresentationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongProbeKind { expected, actual } => {
                write!(formatter, "expected {expected:?} probe, got {actual:?}")
            }
            Self::OwnerMismatch => formatter.write_str("game catalog probe owner does not match"),
            Self::CatalogTooLarge { actual, maximum } => {
                write!(
                    formatter,
                    "game catalog has {actual} rows; maximum is {maximum}"
                )
            }
            Self::InvalidPlugin => formatter.write_str("game catalog contains an invalid plugin"),
            Self::PluginOrder => {
                formatter.write_str("game plugins are not in stable registry order")
            }
            Self::InvalidCustom => {
                formatter.write_str("game catalog contains an invalid custom game")
            }
            Self::DuplicateIdentity => {
                formatter.write_str("game catalog contains a duplicate identity")
            }
            Self::InvalidSelection => {
                formatter.write_str("game selection is not sorted, unique, and current")
            }
            Self::InvalidIconIdentity => formatter.write_str("game row icon identity is invalid"),
            Self::Allocation { field } => {
                write!(formatter, "could not reserve bounded storage for {field}")
            }
        }
    }
}

impl Error for GamePresentationError {}

pub trait GameProjectionReservation {
    fn before_reserve(
        &self,
        field: &'static str,
        additional: usize,
    ) -> Result<(), GamePresentationError>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemGameProjectionReservation;

impl GameProjectionReservation for SystemGameProjectionReservation {
    fn before_reserve(
        &self,
        _field: &'static str,
        _additional: usize,
    ) -> Result<(), GamePresentationError> {
        Ok(())
    }
}

#[derive(Debug)]
pub enum GameCandidateCatalog {
    Installed(InstalledGameIdentityCatalog),
    RunningWindows(GameWindowIdentityCatalog),
}

impl GameCandidateCatalog {
    fn token(&self) -> ProbeToken {
        match self {
            Self::Installed(catalog) => catalog.token(),
            Self::RunningWindows(catalog) => catalog.token(),
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Installed(catalog) => catalog.len(),
            Self::RunningWindows(catalog) => catalog.len(),
        }
    }
}

#[derive(Debug)]
pub struct GameCatalogInput {
    pub owner: ProbeSessionOwner,
    pub plugins_token: ProbeToken,
    pub plugins: Vec<GamePluginInfo>,
    pub settings: GamePreferences,
    pub candidates: Option<GameCandidateCatalog>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameRowKind {
    Plugin,
    Custom,
    InstalledCandidate,
    RunningWindow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemberLocator {
    Plugin(usize),
    Custom(usize),
    Candidate(usize),
}

#[derive(Debug)]
struct CatalogMember {
    identity: GameItemIdentity,
    locator: MemberLocator,
    kind: GameRowKind,
    title: String,
    subtitle: String,
    enabled: Option<bool>,
    recording_mode: Option<GameRecordingMode>,
    has_icon: bool,
}

#[derive(Debug)]
pub enum ResolvedGameCatalogMember<'a> {
    Plugin(&'a GamePluginInfo),
    Custom(&'a CustomGameSettings),
    InstalledCandidate(&'a DetectedGameCandidate),
    RunningWindow(&'a GameWindowInfo),
}

#[derive(Debug)]
pub struct GameCatalog {
    owner: ProbeSessionOwner,
    plugins_token: ProbeToken,
    plugins: Vec<GamePluginInfo>,
    settings: GamePreferences,
    candidates: Option<GameCandidateCatalog>,
    members: Vec<CatalogMember>,
}

impl GameCatalog {
    pub fn try_build(
        input: GameCatalogInput,
        reservation: &dyn GameProjectionReservation,
    ) -> Result<Self, GamePresentationError> {
        validate_token(input.owner, input.plugins_token, ProbeKind::GamePlugins)?;
        if let Some(candidates) = input.candidates.as_ref() {
            let token = candidates.token();
            if token.owner != input.owner {
                return Err(GamePresentationError::OwnerMismatch);
            }
        }
        if input.plugins.len() > MAX_SETTINGS_GAME_PLUGINS
            || input.settings.custom_games.len() > MAX_SETTINGS_CUSTOM_GAMES
        {
            return Err(GamePresentationError::CatalogTooLarge {
                actual: input
                    .plugins
                    .len()
                    .saturating_add(input.settings.custom_games.len()),
                maximum: MAX_GAME_CATALOG_ROWS,
            });
        }

        validate_plugin_order(&input.plugins)?;
        let candidate_count = input
            .candidates
            .as_ref()
            .map_or(0, GameCandidateCatalog::len);
        let maximum_total = input
            .plugins
            .len()
            .checked_add(input.settings.custom_games.len())
            .and_then(|count| count.checked_add(candidate_count))
            .ok_or(GamePresentationError::CatalogTooLarge {
                actual: usize::MAX,
                maximum: MAX_GAME_CATALOG_ROWS,
            })?;
        if maximum_total > MAX_GAME_CATALOG_ROWS {
            return Err(GamePresentationError::CatalogTooLarge {
                actual: maximum_total,
                maximum: MAX_GAME_CATALOG_ROWS,
            });
        }

        reservation.before_reserve("games.catalog_members", maximum_total)?;
        let mut members = Vec::new();
        members.try_reserve_exact(maximum_total).map_err(|_| {
            GamePresentationError::Allocation {
                field: "games.catalog_members",
            }
        })?;

        for (index, plugin) in input.plugins.iter().enumerate() {
            let identity = GameItemIdentity::Plugin(
                PluginGameIdentity::new(&plugin.id)
                    .map_err(|_| GamePresentationError::InvalidPlugin)?,
            );
            ensure_unique(&members, &identity)?;
            let configured = input
                .settings
                .plugins
                .iter()
                .find(|configured| configured.id == plugin.id)
                .map(|configured| &configured.settings);
            members.push(CatalogMember {
                identity,
                locator: MemberLocator::Plugin(index),
                kind: GameRowKind::Plugin,
                title: bounded_text(
                    &plugin.name,
                    "Supported game",
                    "games.plugin_title",
                    reservation,
                )?,
                subtitle: bounded_text(
                    &plugin.summary,
                    "Built-in support",
                    "games.plugin_subtitle",
                    reservation,
                )?,
                enabled: Some(configured.map_or(plugin.default_enabled, |value| value.enabled)),
                recording_mode: Some(
                    configured.map_or(plugin.default_recording_mode, |value| value.recording_mode),
                ),
                has_icon: plugin
                    .icon
                    .as_deref()
                    .is_some_and(|value| !value.is_empty()),
            });
        }

        for (index, custom) in input.settings.custom_games.iter().enumerate() {
            let identity = GameItemIdentity::Custom(
                CustomGameIdentity::new(&custom.id)
                    .map_err(|_| GamePresentationError::InvalidCustom)?,
            );
            ensure_unique(&members, &identity)?;
            members.push(CatalogMember {
                identity,
                locator: MemberLocator::Custom(index),
                kind: GameRowKind::Custom,
                title: bounded_text(
                    &custom.name,
                    "Custom game",
                    "games.custom_title",
                    reservation,
                )?,
                subtitle: joined_text(
                    [&custom.exe_name, &custom.window_title],
                    "Custom detection rule",
                    "games.custom_subtitle",
                    reservation,
                )?,
                enabled: Some(custom.enabled),
                recording_mode: Some(custom.recording_mode),
                has_icon: custom
                    .icon
                    .as_deref()
                    .is_some_and(|value| !value.is_empty()),
            });
        }

        if let Some(candidates) = input.candidates.as_ref() {
            match candidates {
                GameCandidateCatalog::Installed(catalog) => {
                    for (index, (identity, source)) in catalog.iter().enumerate() {
                        if input
                            .settings
                            .custom_games
                            .iter()
                            .any(|custom| matches_existing_custom_game(source, custom))
                        {
                            continue;
                        }
                        let identity = GameItemIdentity::Candidate(identity.clone());
                        ensure_unique(&members, &identity)?;
                        members.push(CatalogMember {
                            identity,
                            locator: MemberLocator::Candidate(index),
                            kind: GameRowKind::InstalledCandidate,
                            title: bounded_text(
                                &source.name,
                                "Detected game",
                                "games.candidate_title",
                                reservation,
                            )?,
                            subtitle: joined_text(
                                [candidate_source_label(source), source.exe_name.as_str()],
                                "Detected game",
                                "games.candidate_subtitle",
                                reservation,
                            )?,
                            enabled: None,
                            recording_mode: None,
                            has_icon: source
                                .icon
                                .as_deref()
                                .is_some_and(|value| !value.is_empty()),
                        });
                    }
                }
                GameCandidateCatalog::RunningWindows(catalog) => {
                    for (index, (identity, source)) in catalog.iter().enumerate() {
                        if input.settings.custom_games.iter().any(|custom| {
                            discovery_fields_match_custom_game(
                                source.exe_path.as_deref(),
                                &source.exe_name,
                                &source.title,
                                custom,
                            )
                        }) {
                            continue;
                        }
                        let identity = GameItemIdentity::Candidate(identity.clone());
                        ensure_unique(&members, &identity)?;
                        members.push(CatalogMember {
                            identity,
                            locator: MemberLocator::Candidate(index),
                            kind: GameRowKind::RunningWindow,
                            title: bounded_text(
                                &source.title,
                                "Running window",
                                "games.window_title",
                                reservation,
                            )?,
                            subtitle: window_subtitle(source, reservation)?,
                            enabled: None,
                            recording_mode: None,
                            has_icon: false,
                        });
                    }
                }
            }
        }

        Ok(Self {
            owner: input.owner,
            plugins_token: input.plugins_token,
            plugins: input.plugins,
            settings: input.settings,
            candidates: input.candidates,
            members,
        })
    }

    pub const fn owner(&self) -> ProbeSessionOwner {
        self.owner
    }

    pub const fn plugins_token(&self) -> ProbeToken {
        self.plugins_token
    }

    pub fn candidate_token(&self) -> Option<ProbeToken> {
        self.candidates.as_ref().map(GameCandidateCatalog::token)
    }

    /// Returns every owned catalog input without cloning the bounded source
    /// collections or rebuilding candidate authority.
    ///
    /// Controllers use this consuming seam when replacing one exact probe
    /// token or switching candidate sources. Candidate identity/source
    /// membership remains owned by the returned catalog.
    pub fn into_input(self) -> GameCatalogInput {
        GameCatalogInput {
            owner: self.owner,
            plugins_token: self.plugins_token,
            plugins: self.plugins,
            settings: self.settings,
            candidates: self.candidates,
        }
    }

    pub fn len(&self) -> usize {
        self.members.len()
    }

    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    pub fn identities(&self) -> impl ExactSizeIterator<Item = &GameItemIdentity> {
        self.members.iter().map(|member| &member.identity)
    }

    pub fn resolve(&self, identity: &GameItemIdentity) -> Option<ResolvedGameCatalogMember<'_>> {
        let member = self
            .members
            .iter()
            .find(|member| &member.identity == identity)?;
        match member.locator {
            MemberLocator::Plugin(index) => self
                .plugins
                .get(index)
                .map(ResolvedGameCatalogMember::Plugin),
            MemberLocator::Custom(index) => self
                .settings
                .custom_games
                .get(index)
                .map(ResolvedGameCatalogMember::Custom),
            MemberLocator::Candidate(index) => match self.candidates.as_ref()? {
                GameCandidateCatalog::Installed(catalog) => catalog
                    .source_at(index)
                    .map(ResolvedGameCatalogMember::InstalledCandidate),
                GameCandidateCatalog::RunningWindows(catalog) => catalog
                    .source_at(index)
                    .map(ResolvedGameCatalogMember::RunningWindow),
            },
        }
    }

    pub fn project_page<F>(
        &self,
        requested: GamePageIndex,
        selected: &[GameItemIdentity],
        mut icon_state: F,
        reservation: &dyn GameProjectionReservation,
    ) -> Result<GamePageOutcome, GamePresentationError>
    where
        F: FnMut(&GameIconId) -> GameIconLoadState,
    {
        validate_selection(self, selected)?;
        let window = game_page_window(self.members.len(), requested);
        let GamePageWindow::Page {
            page_count,
            start,
            end,
        } = window
        else {
            let GamePageWindow::PastEnd { fallback_page, .. } = window else {
                unreachable!()
            };
            return Ok(GamePageOutcome::PastEnd {
                owner: self.owner,
                requested_page: requested,
                fallback_page,
                total: self.members.len(),
                page_count: game_page_count(self.members.len()),
            });
        };

        reservation.before_reserve("games.page_rows", end.saturating_sub(start))?;
        let mut rows = Vec::new();
        rows.try_reserve_exact(end.saturating_sub(start))
            .map_err(|_| GamePresentationError::Allocation {
                field: "games.page_rows",
            })?;
        for member in &self.members[start..end] {
            let (icon_id, state) = if member.has_icon {
                let id = GameIconId::new(self.owner, member.identity.clone())
                    .map_err(|_| GamePresentationError::InvalidIconIdentity)?;
                let state = icon_state(&id);
                (Some(id), state)
            } else {
                (None, GameIconLoadState::Missing)
            };
            rows.push(GameRowProjection {
                identity: member.identity.clone(),
                kind: member.kind,
                title: clone_bounded_text(&member.title, "games.page_title", reservation)?,
                subtitle: clone_bounded_text(&member.subtitle, "games.page_subtitle", reservation)?,
                enabled: member.enabled,
                recording_mode: member.recording_mode,
                selected: selected.binary_search(&member.identity).is_ok(),
                icon_id,
                icon_state: state,
            });
        }
        Ok(GamePageOutcome::Page(GamePageProjection {
            owner: self.owner,
            page: requested,
            page_count,
            total: self.members.len(),
            start,
            end,
            has_previous: requested.get() > 0,
            has_next: usize::try_from(requested.get())
                .ok()
                .is_some_and(|page| page.saturating_add(1) < page_count),
            rows,
        }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GamePageIndex(u32);

impl GamePageIndex {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameRowProjection {
    pub identity: GameItemIdentity,
    pub kind: GameRowKind,
    pub title: String,
    pub subtitle: String,
    pub enabled: Option<bool>,
    pub recording_mode: Option<GameRecordingMode>,
    pub selected: bool,
    pub icon_id: Option<GameIconId>,
    pub icon_state: GameIconLoadState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GamePageProjection {
    pub owner: ProbeSessionOwner,
    pub page: GamePageIndex,
    pub page_count: usize,
    pub total: usize,
    pub start: usize,
    pub end: usize,
    pub has_previous: bool,
    pub has_next: bool,
    pub rows: Vec<GameRowProjection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GamePageOutcome {
    Page(GamePageProjection),
    PastEnd {
        owner: ProbeSessionOwner,
        requested_page: GamePageIndex,
        fallback_page: Option<GamePageIndex>,
        total: usize,
        page_count: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GamePageWindow {
    Page {
        page_count: usize,
        start: usize,
        end: usize,
    },
    PastEnd {
        page_count: usize,
        fallback_page: Option<GamePageIndex>,
    },
}

pub fn game_page_count(total: usize) -> usize {
    total / MAX_GAME_PAGE_ROWS + usize::from(!total.is_multiple_of(MAX_GAME_PAGE_ROWS))
}

pub fn game_page_window(total: usize, requested: GamePageIndex) -> GamePageWindow {
    let page_count = game_page_count(total);
    let page = u64::from(requested.get());
    if total == 0 && page == 0 {
        return GamePageWindow::Page {
            page_count: 0,
            start: 0,
            end: 0,
        };
    }
    if page >= u64::try_from(page_count).unwrap_or(u64::MAX) {
        return GamePageWindow::PastEnd {
            page_count,
            fallback_page: page_count
                .checked_sub(1)
                .and_then(|value| u32::try_from(value).ok())
                .map(GamePageIndex::new),
        };
    }
    let start = usize::try_from(page)
        .ok()
        .and_then(|page| page.checked_mul(MAX_GAME_PAGE_ROWS))
        .unwrap_or(total);
    GamePageWindow::Page {
        page_count,
        start,
        end: total.min(start.saturating_add(MAX_GAME_PAGE_ROWS)),
    }
}

fn validate_token(
    owner: ProbeSessionOwner,
    token: ProbeToken,
    expected: ProbeKind,
) -> Result<(), GamePresentationError> {
    if token.kind != expected {
        return Err(GamePresentationError::WrongProbeKind {
            expected,
            actual: token.kind,
        });
    }
    if token.owner != owner {
        return Err(GamePresentationError::OwnerMismatch);
    }
    Ok(())
}

fn validate_plugin_order(plugins: &[GamePluginInfo]) -> Result<(), GamePresentationError> {
    let registry = crate::plugin::all();
    let mut previous = None;
    for plugin in plugins {
        let position = registry
            .iter()
            .position(|registered| registered.id() == plugin.id)
            .ok_or(GamePresentationError::InvalidPlugin)?;
        if previous.is_some_and(|previous| previous >= position) {
            return Err(GamePresentationError::PluginOrder);
        }
        previous = Some(position);
    }
    Ok(())
}

fn ensure_unique(
    members: &[CatalogMember],
    identity: &GameItemIdentity,
) -> Result<(), GamePresentationError> {
    if members.iter().any(|member| &member.identity == identity) {
        Err(GamePresentationError::DuplicateIdentity)
    } else {
        Ok(())
    }
}

fn validate_selection(
    catalog: &GameCatalog,
    selected: &[GameItemIdentity],
) -> Result<(), GamePresentationError> {
    if selected.windows(2).any(|pair| pair[0] >= pair[1])
        || selected.iter().any(|identity| {
            !matches!(identity, GameItemIdentity::Candidate(_))
                || catalog.resolve(identity).is_none()
        })
    {
        return Err(GamePresentationError::InvalidSelection);
    }
    Ok(())
}

fn bounded_text(
    value: &str,
    fallback: &str,
    field: &'static str,
    reservation: &dyn GameProjectionReservation,
) -> Result<String, GamePresentationError> {
    let value = if value.trim().is_empty() {
        fallback
    } else {
        value.trim()
    };
    let end = utf8_prefix(value, MAX_GAME_ROW_TEXT_BYTES);
    clone_bounded_text(&value[..end], field, reservation)
}

fn clone_bounded_text(
    value: &str,
    field: &'static str,
    reservation: &dyn GameProjectionReservation,
) -> Result<String, GamePresentationError> {
    reservation.before_reserve(field, value.len())?;
    let mut output = String::new();
    output
        .try_reserve_exact(value.len())
        .map_err(|_| GamePresentationError::Allocation { field })?;
    output.push_str(value);
    Ok(output)
}

fn joined_text<const N: usize>(
    values: [&str; N],
    fallback: &str,
    field: &'static str,
    reservation: &dyn GameProjectionReservation,
) -> Result<String, GamePresentationError> {
    if values.iter().all(|value| value.trim().is_empty()) {
        return bounded_text(fallback, fallback, field, reservation);
    }
    reservation.before_reserve(field, MAX_GAME_ROW_TEXT_BYTES)?;
    let mut output = String::new();
    output
        .try_reserve_exact(MAX_GAME_ROW_TEXT_BYTES)
        .map_err(|_| GamePresentationError::Allocation { field })?;
    for value in values
        .into_iter()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if !output.is_empty() {
            push_bounded(&mut output, " · ");
        }
        push_bounded(&mut output, value);
        if output.len() == MAX_GAME_ROW_TEXT_BYTES {
            break;
        }
    }
    Ok(output)
}

fn push_bounded(output: &mut String, value: &str) {
    let remaining = MAX_GAME_ROW_TEXT_BYTES.saturating_sub(output.len());
    let end = utf8_prefix(value, remaining);
    output.push_str(&value[..end]);
}

fn utf8_prefix(value: &str, maximum: usize) -> usize {
    let mut end = value.len().min(maximum);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn candidate_source_label(candidate: &DetectedGameCandidate) -> &'static str {
    use crate::discovery::DetectedGameSource;
    match candidate.source {
        DetectedGameSource::Steam => "Steam",
        DetectedGameSource::RunningWindow => "Running window",
        DetectedGameSource::SteamAndRunningWindow => "Steam and running",
    }
}

fn window_subtitle(
    window: &GameWindowInfo,
    reservation: &dyn GameProjectionReservation,
) -> Result<String, GamePresentationError> {
    let pid = window.process_id.to_string();
    joined_text(
        [window.exe_name.as_str(), pid.as_str()],
        "Running window",
        "games.window_subtitle",
        reservation,
    )
}
