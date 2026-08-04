//! Bounded, frontend-neutral result delivery for the native Games controller.

use std::collections::VecDeque;
use std::fmt;
use std::mem::size_of;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::icon::{DecodedGameIcon, GameIconWork};
use crate::identity::{GameWindowIdentityCatalog, InstalledGameIdentityCatalog};
use crate::plugin::{validate_plugin_catalog, GamePluginInfo};
use clipline_settings::{ProbeKind, ProbeSessionOwner, ProbeToken};

pub const GAMES_RESULT_CAPACITY: usize = 64;
pub const GAMES_RESULT_NORMAL_CAPACITY: usize = 60;
pub const GAMES_RESULT_RESERVED_CAPACITY: usize = 4;
pub const GAMES_RESULT_BYTE_CAPACITY: usize = 32 * 1024 * 1024;
pub const MAX_GAMES_RESULT_ERROR_BYTES: usize = 64 * 1024;
pub const GAMES_RESULT_RESERVED_BYTE_CAPACITY: usize =
    GAMES_RESULT_RESERVED_CAPACITY * (MAX_GAMES_RESULT_ERROR_BYTES + size_of::<GamesProbeFailed>());
pub const GAMES_RESULT_NORMAL_BYTE_CAPACITY: usize =
    GAMES_RESULT_BYTE_CAPACITY - GAMES_RESULT_RESERVED_BYTE_CAPACITY;
// Candidate catalogs own both the accepted Task 5 source strings and a second
// complete canonical-authority framing. These are conservative ownership
// ceilings, not serialized payload estimates.
const PLUGIN_CATALOG_ACCOUNTING_BYTES: usize = 5 * 1024 * 1024;
const CANDIDATE_CATALOG_ACCOUNTING_BYTES: usize = 9 * 1024 * 1024;

const _: () = {
    assert!(GAMES_RESULT_NORMAL_CAPACITY + GAMES_RESULT_RESERVED_CAPACITY == GAMES_RESULT_CAPACITY);
    assert!(
        GAMES_RESULT_NORMAL_BYTE_CAPACITY + GAMES_RESULT_RESERVED_BYTE_CAPACITY
            == GAMES_RESULT_BYTE_CAPACITY
    );
    assert!(
        GAMES_RESULT_BYTE_CAPACITY
            >= PLUGIN_CATALOG_ACCOUNTING_BYTES + (2 * CANDIDATE_CATALOG_ACCOUNTING_BYTES)
    );
};

pub enum GamesProbeCatalog {
    Plugins(Vec<GamePluginInfo>),
    Installed(InstalledGameIdentityCatalog),
    RunningWindows(GameWindowIdentityCatalog),
}

impl GamesProbeCatalog {
    pub const fn kind(&self) -> ProbeKind {
        match self {
            Self::Plugins(_) => ProbeKind::GamePlugins,
            Self::Installed(_) => ProbeKind::InstalledGames,
            Self::RunningWindows(_) => ProbeKind::GameWindows,
        }
    }

    fn token(&self) -> Option<ProbeToken> {
        match self {
            Self::Plugins(_) => None,
            Self::Installed(catalog) => Some(catalog.token()),
            Self::RunningWindows(catalog) => Some(catalog.token()),
        }
    }
}

impl fmt::Debug for GamesProbeCatalog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GamesProbeCatalog")
            .field("kind", &self.kind())
            .finish_non_exhaustive()
    }
}

pub struct GamesProbeReady {
    token: ProbeToken,
    catalog: GamesProbeCatalog,
}

impl GamesProbeReady {
    pub fn new(token: ProbeToken, catalog: GamesProbeCatalog) -> Result<Self, GamesResultError> {
        if token.kind != catalog.kind()
            || catalog.token().is_some_and(|stored| stored != token)
            || matches!(&catalog, GamesProbeCatalog::Plugins(plugins) if validate_plugin_catalog(plugins).is_err())
        {
            return Err(GamesResultError::InvalidPayload);
        }
        Ok(Self { token, catalog })
    }

    pub const fn token(&self) -> ProbeToken {
        self.token
    }

    pub const fn catalog(&self) -> &GamesProbeCatalog {
        &self.catalog
    }

    pub fn into_catalog(self) -> GamesProbeCatalog {
        self.catalog
    }
}

impl fmt::Debug for GamesProbeReady {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GamesProbeReady")
            .field("token", &self.token)
            .field("catalog", &self.catalog)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct GamesProbeFailed {
    token: ProbeToken,
    error: String,
}

impl GamesProbeFailed {
    pub fn new(token: ProbeToken, error: String) -> Result<Self, GamesResultError> {
        if !matches!(
            token.kind,
            ProbeKind::GamePlugins | ProbeKind::InstalledGames | ProbeKind::GameWindows
        ) || error.is_empty()
            || error.len() > MAX_GAMES_RESULT_ERROR_BYTES
        {
            return Err(GamesResultError::InvalidPayload);
        }
        Ok(Self { token, error })
    }

    pub const fn token(&self) -> ProbeToken {
        self.token
    }

    pub fn error(&self) -> &str {
        &self.error
    }
}

impl fmt::Debug for GamesProbeFailed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GamesProbeFailed")
            .field("token", &self.token)
            .field("error_bytes", &self.error.len())
            .finish()
    }
}

pub enum GameIconWorkerResult {
    Decoded {
        work: GameIconWork,
        decoded: DecodedGameIcon,
    },
    Failed {
        work: GameIconWork,
    },
}

impl GameIconWorkerResult {
    pub const fn work(&self) -> &GameIconWork {
        match self {
            Self::Decoded { work, .. } | Self::Failed { work } => work,
        }
    }

    pub fn into_parts(self) -> (GameIconWork, Option<DecodedGameIcon>) {
        match self {
            Self::Decoded { work, decoded } => (work, Some(decoded)),
            Self::Failed { work } => (work, None),
        }
    }
}

impl fmt::Debug for GameIconWorkerResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decoded { work, decoded } => formatter
                .debug_struct("DecodedGameIconResult")
                .field("work", work)
                .field("width", &decoded.width())
                .field("height", &decoded.height())
                .field("rgba_bytes", &decoded.rgba().len())
                .finish(),
            Self::Failed { work } => formatter
                .debug_struct("FailedGameIconResult")
                .field("work", work)
                .finish(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GamesBarrier {
    DraftReplaced {
        previous: ProbeSessionOwner,
        current: ProbeSessionOwner,
    },
    DraftSaved {
        owner: ProbeSessionOwner,
    },
    DraftDiscarded {
        owner: ProbeSessionOwner,
    },
    Detached {
        owner: ProbeSessionOwner,
    },
    Shutdown {
        owner: ProbeSessionOwner,
    },
}

impl GamesBarrier {
    pub const fn draft_replace(previous: ProbeSessionOwner, current: ProbeSessionOwner) -> Self {
        Self::DraftReplaced { previous, current }
    }

    pub const fn draft_save(owner: ProbeSessionOwner) -> Self {
        Self::DraftSaved { owner }
    }

    pub const fn draft_discard(owner: ProbeSessionOwner) -> Self {
        Self::DraftDiscarded { owner }
    }

    pub const fn detached(owner: ProbeSessionOwner) -> Self {
        Self::Detached { owner }
    }

    pub const fn shutdown(owner: ProbeSessionOwner) -> Self {
        Self::Shutdown { owner }
    }
}

pub enum GamesResult {
    ProbeReady(GamesProbeReady),
    ProbeFailed(GamesProbeFailed),
    Icon(GameIconWorkerResult),
    Barrier(GamesBarrier),
    #[cfg(test)]
    SyntheticNormal {
        owner: ProbeSessionOwner,
        sequence: u64,
        bytes: usize,
    },
    #[cfg(test)]
    SyntheticReserved {
        owner: ProbeSessionOwner,
        sequence: u64,
        bytes: usize,
    },
}

impl fmt::Debug for GamesResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProbeReady(result) => result.fmt(formatter),
            Self::ProbeFailed(result) => result.fmt(formatter),
            Self::Icon(result) => result.fmt(formatter),
            Self::Barrier(barrier) => barrier.fmt(formatter),
            #[cfg(test)]
            Self::SyntheticNormal {
                owner, sequence, ..
            } => formatter
                .debug_struct("SyntheticNormal")
                .field("owner", owner)
                .field("sequence", sequence)
                .finish(),
            #[cfg(test)]
            Self::SyntheticReserved {
                owner, sequence, ..
            } => formatter
                .debug_struct("SyntheticReserved")
                .field("owner", owner)
                .field("sequence", sequence)
                .finish(),
        }
    }
}

impl GamesResult {
    fn owner(&self) -> ProbeSessionOwner {
        match self {
            Self::ProbeReady(result) => result.token.owner,
            Self::ProbeFailed(result) => result.token.owner,
            Self::Icon(result) => result.work().id().owner(),
            Self::Barrier(barrier) => barrier_owner(*barrier),
            #[cfg(test)]
            Self::SyntheticNormal { owner, .. } | Self::SyntheticReserved { owner, .. } => *owner,
        }
    }

    fn probe_token(&self) -> Option<ProbeToken> {
        match self {
            Self::ProbeReady(result) => Some(result.token),
            Self::ProbeFailed(result) => Some(result.token),
            Self::Icon(_) | Self::Barrier(_) => None,
            #[cfg(test)]
            Self::SyntheticNormal { .. } | Self::SyntheticReserved { .. } => None,
        }
    }

    fn is_reserved(&self) -> bool {
        match self {
            Self::ProbeFailed(_) | Self::Barrier(_) => true,
            #[cfg(test)]
            Self::SyntheticReserved { .. } => true,
            _ => false,
        }
    }

    fn estimated_bytes(&self) -> usize {
        match self {
            Self::ProbeReady(result) => match &result.catalog {
                GamesProbeCatalog::Plugins(_) => PLUGIN_CATALOG_ACCOUNTING_BYTES,
                GamesProbeCatalog::Installed(_) | GamesProbeCatalog::RunningWindows(_) => {
                    CANDIDATE_CATALOG_ACCOUNTING_BYTES
                }
            },
            Self::ProbeFailed(result) => size_of::<GamesProbeFailed>() + result.error.len(),
            Self::Icon(GameIconWorkerResult::Decoded { work, decoded }) => {
                size_of::<GameIconWorkerResult>()
                    .saturating_add(icon_work_source_bytes(work))
                    .saturating_add(decoded.rgba().len())
            }
            Self::Icon(GameIconWorkerResult::Failed { work }) => {
                size_of::<GameIconWorkerResult>().saturating_add(icon_work_source_bytes(work))
            }
            Self::Barrier(_) => size_of::<GamesBarrier>(),
            #[cfg(test)]
            Self::SyntheticNormal { bytes, .. } | Self::SyntheticReserved { bytes, .. } => *bytes,
        }
    }
}

fn icon_work_source_bytes(work: &GameIconWork) -> usize {
    work.source()
        .as_png_data_url()
        .or_else(|| work.source().as_first_party_asset_path())
        .map_or(0, str::len)
}

fn barrier_owner(barrier: GamesBarrier) -> ProbeSessionOwner {
    match barrier {
        GamesBarrier::DraftReplaced { previous, .. } => previous,
        GamesBarrier::DraftSaved { owner }
        | GamesBarrier::DraftDiscarded { owner }
        | GamesBarrier::Detached { owner }
        | GamesBarrier::Shutdown { owner } => owner,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GamesResultError {
    Stale,
    StaleOwner,
    InvalidPayload,
    Full { capacity: usize },
    ByteCapacity { capacity: usize },
    Disconnected,
}

impl fmt::Display for GamesResultError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stale => formatter.write_str("Games result belongs to stale probe work"),
            Self::StaleOwner => {
                formatter.write_str("Games result belongs to a replaced Settings owner")
            }
            Self::InvalidPayload => formatter.write_str("Games result payload is invalid"),
            Self::Full { capacity } => {
                write!(
                    formatter,
                    "Games result queue is full at capacity {capacity}"
                )
            }
            Self::ByteCapacity { capacity } => write!(
                formatter,
                "Games result queue is full at byte capacity {capacity}"
            ),
            Self::Disconnected => formatter.write_str("Games result channel is disconnected"),
        }
    }
}

impl std::error::Error for GamesResultError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GamesResultPublishOutcome {
    Queued,
    Replaced,
}

pub struct RejectedGamesResult {
    pub error: GamesResultError,
    pub result: GamesResult,
}

impl fmt::Debug for RejectedGamesResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RejectedGamesResult")
            .field("error", &self.error)
            .field("result", &self.result)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdmissionClass {
    Normal,
    Reserved,
}

struct QueuedResult {
    result: GamesResult,
    admission: AdmissionClass,
}

impl QueuedResult {
    fn byte_class(&self) -> AdmissionClass {
        if self.admission == AdmissionClass::Reserved && self.result.is_reserved() {
            AdmissionClass::Reserved
        } else {
            AdmissionClass::Normal
        }
    }
}

struct ChannelState {
    owner: ProbeSessionOwner,
    latest_probe_generation: [u64; ProbeKind::COUNT],
    queue: VecDeque<QueuedResult>,
    normal_queue_bytes: usize,
    reserved_queue_bytes: usize,
    receiver_connected: bool,
    sender_count: usize,
}

struct Shared {
    state: Mutex<ChannelState>,
    ready: Condvar,
}

pub struct GamesResultSender {
    shared: Arc<Shared>,
}

pub struct GamesResultReceiver {
    shared: Arc<Shared>,
}

#[must_use]
pub fn games_result_channel(owner: ProbeSessionOwner) -> (GamesResultSender, GamesResultReceiver) {
    let shared = Arc::new(Shared {
        state: Mutex::new(ChannelState {
            owner,
            latest_probe_generation: [0; ProbeKind::COUNT],
            queue: VecDeque::with_capacity(GAMES_RESULT_CAPACITY),
            normal_queue_bytes: 0,
            reserved_queue_bytes: 0,
            receiver_connected: true,
            sender_count: 1,
        }),
        ready: Condvar::new(),
    });
    (
        GamesResultSender {
            shared: Arc::clone(&shared),
        },
        GamesResultReceiver { shared },
    )
}

impl Clone for GamesResultSender {
    fn clone(&self) -> Self {
        if let Ok(mut state) = self.shared.state.lock() {
            state.sender_count = state.sender_count.saturating_add(1);
        }
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl GamesResultSender {
    pub fn try_send(
        &self,
        result: GamesResult,
    ) -> Result<GamesResultPublishOutcome, Box<RejectedGamesResult>> {
        macro_rules! reject {
            ($error:expr) => {
                return Err(Box::new(RejectedGamesResult {
                    error: $error,
                    result,
                }))
            };
        }

        let mut state = match self.shared.state.lock() {
            Ok(state) => state,
            Err(_) => reject!(GamesResultError::Disconnected),
        };
        if !state.receiver_connected {
            reject!(GamesResultError::Disconnected);
        }
        if result.owner() != state.owner {
            reject!(GamesResultError::StaleOwner);
        }
        if let GamesResult::Barrier(GamesBarrier::DraftReplaced { previous, current }) = &result {
            if current <= previous {
                reject!(GamesResultError::InvalidPayload);
            }
        }

        let replacement = if let Some(token) = result.probe_token() {
            let kind_index = token.kind as usize;
            if token.request_generation.get() <= state.latest_probe_generation[kind_index] {
                reject!(GamesResultError::Stale);
            }
            state
                .queue
                .iter()
                .enumerate()
                .rev()
                .take_while(|(_, queued)| !matches!(queued.result, GamesResult::Barrier(_)))
                .find_map(|(index, queued)| {
                    queued
                        .result
                        .probe_token()
                        .is_some_and(|queued| {
                            queued.owner == token.owner && queued.kind == token.kind
                        })
                        .then_some(index)
                })
        } else {
            None
        };

        let initial_admission = if result.is_reserved() {
            AdmissionClass::Reserved
        } else {
            AdmissionClass::Normal
        };
        let admission = replacement
            .and_then(|index| state.queue.get(index))
            .map_or(initial_admission, |queued| queued.admission);
        let normal_count = state
            .queue
            .iter()
            .filter(|queued| queued.admission == AdmissionClass::Normal)
            .count();
        let reserved_count = state
            .queue
            .iter()
            .filter(|queued| queued.admission == AdmissionClass::Reserved)
            .count();
        let projected_normal_count = normal_count.saturating_add(usize::from(
            replacement.is_none() && admission == AdmissionClass::Normal,
        ));
        let projected_reserved_count = reserved_count.saturating_add(usize::from(
            replacement.is_none() && admission == AdmissionClass::Reserved,
        ));
        let projected_count = state
            .queue
            .len()
            .saturating_sub(usize::from(replacement.is_some()))
            .saturating_add(1);
        if projected_normal_count > GAMES_RESULT_NORMAL_CAPACITY
            || projected_reserved_count > GAMES_RESULT_RESERVED_CAPACITY
            || projected_count > GAMES_RESULT_CAPACITY
        {
            reject!(GamesResultError::Full {
                capacity: if projected_count > GAMES_RESULT_CAPACITY {
                    GAMES_RESULT_CAPACITY
                } else if projected_reserved_count > GAMES_RESULT_RESERVED_CAPACITY {
                    GAMES_RESULT_RESERVED_CAPACITY
                } else {
                    GAMES_RESULT_NORMAL_CAPACITY
                },
            });
        }

        let result_bytes = result.estimated_bytes();
        let result_byte_class = if admission == AdmissionClass::Reserved && result.is_reserved() {
            AdmissionClass::Reserved
        } else {
            AdmissionClass::Normal
        };
        let mut projected_normal_bytes = state.normal_queue_bytes;
        let mut projected_reserved_bytes = state.reserved_queue_bytes;
        if let Some(replaced) = replacement.and_then(|index| state.queue.get(index)) {
            match replaced.byte_class() {
                AdmissionClass::Normal => {
                    projected_normal_bytes =
                        projected_normal_bytes.saturating_sub(replaced.result.estimated_bytes());
                }
                AdmissionClass::Reserved => {
                    projected_reserved_bytes =
                        projected_reserved_bytes.saturating_sub(replaced.result.estimated_bytes());
                }
            }
        }
        match result_byte_class {
            AdmissionClass::Normal => {
                projected_normal_bytes = projected_normal_bytes.saturating_add(result_bytes);
            }
            AdmissionClass::Reserved => {
                projected_reserved_bytes = projected_reserved_bytes.saturating_add(result_bytes);
            }
        }
        if projected_normal_bytes > GAMES_RESULT_NORMAL_BYTE_CAPACITY
            || projected_reserved_bytes > GAMES_RESULT_RESERVED_BYTE_CAPACITY
        {
            reject!(GamesResultError::ByteCapacity {
                capacity: GAMES_RESULT_BYTE_CAPACITY,
            });
        }

        if let Some(token) = result.probe_token() {
            state.latest_probe_generation[token.kind as usize] = token.request_generation.get();
        }
        let replacement_owner = match &result {
            GamesResult::Barrier(GamesBarrier::DraftReplaced { current, .. }) => Some(*current),
            _ => None,
        };
        let outcome = if let Some(index) = replacement {
            state.queue.remove(index);
            state.queue.push_back(QueuedResult { result, admission });
            GamesResultPublishOutcome::Replaced
        } else {
            state.queue.push_back(QueuedResult { result, admission });
            GamesResultPublishOutcome::Queued
        };
        state.normal_queue_bytes = projected_normal_bytes;
        state.reserved_queue_bytes = projected_reserved_bytes;
        if let Some(owner) = replacement_owner {
            state.owner = owner;
            state.latest_probe_generation.fill(0);
        }
        drop(state);
        self.shared.ready.notify_one();
        Ok(outcome)
    }
}

impl Drop for GamesResultSender {
    fn drop(&mut self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.sender_count = state.sender_count.saturating_sub(1);
        }
        self.shared.ready.notify_all();
    }
}

impl GamesResultReceiver {
    #[must_use]
    pub fn try_recv(&self) -> Option<GamesResult> {
        self.shared.state.lock().ok().and_then(|mut state| {
            let queued = state.queue.pop_front()?;
            match queued.byte_class() {
                AdmissionClass::Normal => {
                    state.normal_queue_bytes = state
                        .normal_queue_bytes
                        .saturating_sub(queued.result.estimated_bytes());
                }
                AdmissionClass::Reserved => {
                    state.reserved_queue_bytes = state
                        .reserved_queue_bytes
                        .saturating_sub(queued.result.estimated_bytes());
                }
            }
            Some(queued.result)
        })
    }

    pub fn wait_recv(&self, timeout: Duration) -> Result<Option<GamesResult>, GamesResultError> {
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| GamesResultError::Disconnected)?;
        if state.queue.is_empty() && state.sender_count != 0 {
            state = self
                .shared
                .ready
                .wait_timeout(state, timeout)
                .map_err(|_| GamesResultError::Disconnected)?
                .0;
        }
        if let Some(queued) = state.queue.pop_front() {
            match queued.byte_class() {
                AdmissionClass::Normal => {
                    state.normal_queue_bytes = state
                        .normal_queue_bytes
                        .saturating_sub(queued.result.estimated_bytes());
                }
                AdmissionClass::Reserved => {
                    state.reserved_queue_bytes = state
                        .reserved_queue_bytes
                        .saturating_sub(queued.result.estimated_bytes());
                }
            }
            Ok(Some(queued.result))
        } else if state.sender_count == 0 {
            Err(GamesResultError::Disconnected)
        } else {
            Ok(None)
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.shared
            .state
            .lock()
            .map_or(0, |state| state.queue.len())
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Drop for GamesResultReceiver {
    fn drop(&mut self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.receiver_connected = false;
            state.queue.clear();
            state.normal_queue_bytes = 0;
            state.reserved_queue_bytes = 0;
        }
        self.shared.ready.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;
    use clipline_settings::{
        ProbeRequestGeneration, SettingsAttachmentGeneration, SettingsForegroundGeneration,
        SettingsSessionGeneration,
    };

    use crate::icon::{
        decode_game_icon_source, GameIconCache, GameIconId, GameIconManifest,
        GameIconManifestEntry, GameIconManifestGeneration, GameIconSource,
    };
    use crate::identity::{CustomGameIdentity, GameItemIdentity};
    use crate::plugin::catalog_bounded;

    fn owner() -> ProbeSessionOwner {
        ProbeSessionOwner::new(
            SettingsSessionGeneration::new(1),
            SettingsAttachmentGeneration::new(2),
            SettingsForegroundGeneration::new(3),
        )
    }

    fn icon_work(owner: ProbeSessionOwner, count: usize) -> Vec<GameIconWork> {
        let mut png = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[1, 2, 3, 255]).unwrap();
        }
        let source =
            GameIconSource::png_data_url(format!("data:image/png;base64,{}", STANDARD.encode(png)))
                .unwrap();
        let entries: Vec<_> = (0..count)
            .map(|index| {
                let id = GameIconId::new(
                    owner,
                    GameItemIdentity::Custom(
                        CustomGameIdentity::new(&format!("custom-channel-{index}")).unwrap(),
                    ),
                )
                .unwrap();
                GameIconManifestEntry::new(id, source.clone())
            })
            .collect();
        let ids: Vec<_> = entries.iter().map(|entry| entry.id().clone()).collect();
        let manifest =
            GameIconManifest::new(owner, GameIconManifestGeneration::new(1).unwrap(), entries)
                .unwrap();
        let mut cache = GameIconCache::<()>::new(owner);
        cache.sync_manifest(manifest).unwrap();
        cache.set_viewport(&ids).unwrap().queued
    }

    fn rejected_icon_work(rejected: Box<RejectedGamesResult>) -> GameIconWork {
        let GamesResult::Icon(result) = rejected.result else {
            panic!("expected returned icon result")
        };
        result.into_parts().0
    }

    fn token(owner: ProbeSessionOwner, generation: u64) -> ProbeToken {
        ProbeToken {
            owner,
            kind: ProbeKind::GamePlugins,
            request_generation: ProbeRequestGeneration::new(generation),
        }
    }

    #[test]
    fn normal_capacity_preserves_four_barrier_slots() {
        let owner = owner();
        let (sender, receiver) = games_result_channel(owner);
        for sequence in 0..GAMES_RESULT_NORMAL_CAPACITY {
            sender
                .try_send(GamesResult::SyntheticNormal {
                    owner,
                    sequence: sequence as u64,
                    bytes: size_of::<GamesResult>(),
                })
                .unwrap();
        }
        assert_eq!(receiver.len(), GAMES_RESULT_NORMAL_CAPACITY);
        assert_eq!(
            sender
                .try_send(GamesResult::SyntheticNormal {
                    owner,
                    sequence: 61,
                    bytes: size_of::<GamesResult>(),
                })
                .unwrap_err()
                .error,
            GamesResultError::Full {
                capacity: GAMES_RESULT_NORMAL_CAPACITY
            }
        );
        for _ in 0..GAMES_RESULT_RESERVED_CAPACITY {
            sender
                .try_send(GamesResult::Barrier(GamesBarrier::draft_save(owner)))
                .unwrap();
        }
        assert_eq!(receiver.len(), GAMES_RESULT_CAPACITY);

        let (sender, _receiver) = games_result_channel(owner);
        for _ in 0..GAMES_RESULT_RESERVED_CAPACITY {
            sender
                .try_send(GamesResult::Barrier(GamesBarrier::draft_save(owner)))
                .unwrap();
        }
        assert_eq!(
            sender
                .try_send(GamesResult::Barrier(GamesBarrier::draft_save(owner)))
                .unwrap_err()
                .error,
            GamesResultError::Full {
                capacity: GAMES_RESULT_RESERVED_CAPACITY
            }
        );
    }

    #[test]
    fn byte_capacity_fails_closed_without_losing_the_rejected_payload() {
        let owner = owner();
        let (sender, _receiver) = games_result_channel(owner);
        sender
            .try_send(GamesResult::SyntheticNormal {
                owner,
                sequence: 1,
                bytes: GAMES_RESULT_NORMAL_BYTE_CAPACITY,
            })
            .unwrap();
        let rejected = sender
            .try_send(GamesResult::SyntheticNormal {
                owner,
                sequence: 2,
                bytes: 1,
            })
            .unwrap_err();
        assert_eq!(
            rejected.error,
            GamesResultError::ByteCapacity {
                capacity: GAMES_RESULT_BYTE_CAPACITY
            }
        );
        assert!(matches!(
            rejected.result,
            GamesResult::SyntheticNormal { sequence: 2, .. }
        ));
    }

    #[test]
    fn normal_byte_ceiling_preserves_four_maximum_reserved_results() {
        let owner = owner();
        let (sender, receiver) = games_result_channel(owner);
        sender
            .try_send(GamesResult::SyntheticNormal {
                owner,
                sequence: 1,
                bytes: GAMES_RESULT_NORMAL_BYTE_CAPACITY,
            })
            .unwrap();
        let reserved_bytes = GAMES_RESULT_RESERVED_BYTE_CAPACITY / GAMES_RESULT_RESERVED_CAPACITY;
        for sequence in 0..GAMES_RESULT_RESERVED_CAPACITY {
            sender
                .try_send(GamesResult::SyntheticReserved {
                    owner,
                    sequence: sequence as u64,
                    bytes: reserved_bytes,
                })
                .unwrap();
        }
        assert_eq!(receiver.len(), 1 + GAMES_RESULT_RESERVED_CAPACITY);
    }

    #[test]
    fn exact_probe_replacement_preserves_its_admitted_slot_class() {
        let owner = owner();
        let (sender, receiver) = games_result_channel(owner);
        for sequence in 0..GAMES_RESULT_NORMAL_CAPACITY {
            sender
                .try_send(GamesResult::SyntheticNormal {
                    owner,
                    sequence: sequence as u64,
                    bytes: size_of::<GamesResult>(),
                })
                .unwrap();
        }
        sender
            .try_send(GamesResult::ProbeFailed(
                GamesProbeFailed::new(token(owner, 1), "failed".into()).unwrap(),
            ))
            .unwrap();
        let ready = GamesProbeReady::new(
            token(owner, 2),
            GamesProbeCatalog::Plugins(catalog_bounded(std::path::Path::new("unused")).unwrap()),
        )
        .unwrap();
        assert_eq!(
            sender.try_send(GamesResult::ProbeReady(ready)).unwrap(),
            GamesResultPublishOutcome::Replaced
        );
        assert_eq!(receiver.len(), GAMES_RESULT_NORMAL_CAPACITY + 1);

        let (sender, receiver) = games_result_channel(owner);
        for sequence in 0..GAMES_RESULT_NORMAL_CAPACITY - 1 {
            sender
                .try_send(GamesResult::SyntheticNormal {
                    owner,
                    sequence: sequence as u64,
                    bytes: size_of::<GamesResult>(),
                })
                .unwrap();
        }
        let ready = GamesProbeReady::new(
            token(owner, 1),
            GamesProbeCatalog::Plugins(catalog_bounded(std::path::Path::new("unused")).unwrap()),
        )
        .unwrap();
        sender.try_send(GamesResult::ProbeReady(ready)).unwrap();
        assert_eq!(
            sender
                .try_send(GamesResult::ProbeFailed(
                    GamesProbeFailed::new(token(owner, 2), "failed".into()).unwrap(),
                ))
                .unwrap(),
            GamesResultPublishOutcome::Replaced
        );
        assert_eq!(receiver.len(), GAMES_RESULT_NORMAL_CAPACITY);
    }

    #[test]
    fn icon_completions_never_coalesce_and_disconnect_returns_decoded_ownership() {
        let owner = owner();
        let mut work = icon_work(owner, 2);
        let second = work.pop().unwrap();
        let first = work.pop().unwrap();
        let (sender, receiver) = games_result_channel(owner);
        sender
            .try_send(GamesResult::Icon(GameIconWorkerResult::Failed {
                work: first,
            }))
            .unwrap();
        sender
            .try_send(GamesResult::Icon(GameIconWorkerResult::Failed {
                work: second,
            }))
            .unwrap();
        assert_eq!(receiver.len(), 2);
        drop(receiver);

        let work = icon_work(owner, 1).pop().unwrap();
        let decoded = decode_game_icon_source(work.source()).unwrap();
        let rejected = sender
            .try_send(GamesResult::Icon(GameIconWorkerResult::Decoded {
                work,
                decoded,
            }))
            .unwrap_err();
        assert_eq!(rejected.error, GamesResultError::Disconnected);
        let GamesResult::Icon(result) = rejected.result else {
            panic!("expected returned icon result")
        };
        let (_, decoded) = result.into_parts();
        assert_eq!(decoded.unwrap().rgba().len(), 4);
    }

    #[test]
    fn every_icon_rejection_returns_the_exact_cache_ticket() {
        let owner = owner();

        let (sender, _receiver) = games_result_channel(owner);
        for sequence in 0..GAMES_RESULT_NORMAL_CAPACITY {
            sender
                .try_send(GamesResult::SyntheticNormal {
                    owner,
                    sequence: sequence as u64,
                    bytes: size_of::<GamesResult>(),
                })
                .unwrap();
        }
        let work = icon_work(owner, 1).pop().unwrap();
        let expected = work.clone();
        let rejected = sender
            .try_send(GamesResult::Icon(GameIconWorkerResult::Failed { work }))
            .unwrap_err();
        assert_eq!(
            rejected.error,
            GamesResultError::Full {
                capacity: GAMES_RESULT_NORMAL_CAPACITY
            }
        );
        assert_eq!(rejected_icon_work(rejected), expected);

        let (sender, _receiver) = games_result_channel(owner);
        sender
            .try_send(GamesResult::SyntheticNormal {
                owner,
                sequence: 1,
                bytes: GAMES_RESULT_NORMAL_BYTE_CAPACITY,
            })
            .unwrap();
        let work = icon_work(owner, 1).pop().unwrap();
        let expected = work.clone();
        let rejected = sender
            .try_send(GamesResult::Icon(GameIconWorkerResult::Failed { work }))
            .unwrap_err();
        assert_eq!(
            rejected.error,
            GamesResultError::ByteCapacity {
                capacity: GAMES_RESULT_BYTE_CAPACITY
            }
        );
        assert_eq!(rejected_icon_work(rejected), expected);

        let other = ProbeSessionOwner::new(
            SettingsSessionGeneration::new(9),
            SettingsAttachmentGeneration::new(2),
            SettingsForegroundGeneration::new(3),
        );
        let (sender, _receiver) = games_result_channel(owner);
        let work = icon_work(other, 1).pop().unwrap();
        let expected = work.clone();
        let rejected = sender
            .try_send(GamesResult::Icon(GameIconWorkerResult::Failed { work }))
            .unwrap_err();
        assert_eq!(rejected.error, GamesResultError::StaleOwner);
        assert_eq!(rejected_icon_work(rejected), expected);
    }
}
