//! Bounded, frontend-neutral game-icon identities and PNG decoding.

use std::error::Error;
use std::fmt;
use std::io::Cursor;
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use clipline_settings::ProbeSessionOwner;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::identity::GameItemIdentity;

pub const MAX_GAME_ICON_ENCODED_PNG_BYTES: usize = 256 * 1024;
pub const MAX_GAME_ICON_BASE64_BYTES: usize = MAX_GAME_ICON_ENCODED_PNG_BYTES.div_ceil(3) * 4;
pub const MAX_GAME_ICON_DATA_URL_BYTES: usize =
    PNG_DATA_URL_PREFIX.len() + MAX_GAME_ICON_BASE64_BYTES;
pub const MAX_GAME_ICON_ASSET_PATH_BYTES: usize = 4 * 1024;
pub const MAX_SOURCE_ICON_DIMENSION: u32 = 1024;
pub const MAX_SOURCE_ICON_PIXELS: u64 = 1_048_576;
pub const MAX_GAME_ICON_DIMENSION: u32 = 256;
pub const MAX_GAME_ICON_RGBA_BYTES: usize = 256 * 1024;
pub const MAX_GAME_ICON_MANIFEST_ENTRIES: usize = 60;
pub const MAX_GAME_ICON_SLOTS: usize = 32;
pub const MAX_GAME_ICON_DECODED_OWNERSHIP_BYTES: usize =
    MAX_GAME_ICON_SLOTS * MAX_GAME_ICON_RGBA_BYTES;

const PNG_DATA_URL_PREFIX: &str = "data:image/png;base64,";
const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const PNG_IHDR_LENGTH: u32 = 13;
const PNG_HEADER_PREFLIGHT_BYTES: usize = 24;
const MAX_SOURCE_RGBA_BYTES: usize =
    MAX_SOURCE_ICON_DIMENSION as usize * MAX_SOURCE_ICON_DIMENSION as usize * 4;
const MAX_BASE64_DECODE_BYTES: usize = MAX_GAME_ICON_ENCODED_PNG_BYTES + 2;
const PNG_DECODER_ALLOCATION_BYTES: usize = MAX_SOURCE_RGBA_BYTES + (256 * 1024);

const _: () = {
    assert!(MAX_SOURCE_ICON_PIXELS == 1_048_576);
    assert!(MAX_GAME_ICON_RGBA_BYTES == 256 * 256 * 4);
    assert!(MAX_GAME_ICON_DATA_URL_BYTES == 349_550);
    assert!(MAX_GAME_ICON_DECODED_OWNERSHIP_BYTES == 8 * 1024 * 1024);
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GameIconError {
    CandidateOwnerMismatch,
    UnsupportedPngDataUrl,
    EncodedSourceTooLarge,
    AssetPathTooLarge,
    InvalidAssetPath,
    MissingSource,
    AssetReadDeferred,
    InvalidBase64,
    EncodedPngTooLarge,
    InvalidPngHeader,
    InvalidSourceDimensions,
    SourceDimensionsTooLarge,
    PngDecodeFailed,
    PngOutputMismatch,
    RgbaOutputTooLarge,
    AllocationFailed(&'static str),
}

impl fmt::Display for GameIconError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CandidateOwnerMismatch => {
                formatter.write_str("game icon candidate belongs to another Settings owner")
            }
            Self::UnsupportedPngDataUrl => {
                formatter.write_str("game icon must use a PNG base64 data URL")
            }
            Self::EncodedSourceTooLarge => formatter.write_str("game icon data URL is too large"),
            Self::AssetPathTooLarge => formatter.write_str("game icon asset path is too large"),
            Self::InvalidAssetPath => formatter.write_str("game icon asset path is invalid"),
            Self::MissingSource => formatter.write_str("game icon source is missing"),
            Self::AssetReadDeferred => {
                formatter.write_str("game icon asset reading belongs to a platform adapter")
            }
            Self::InvalidBase64 => formatter.write_str("game icon base64 is invalid"),
            Self::EncodedPngTooLarge => formatter.write_str("encoded game icon PNG is too large"),
            Self::InvalidPngHeader => formatter.write_str("game icon PNG header is invalid"),
            Self::InvalidSourceDimensions => {
                formatter.write_str("game icon PNG dimensions are invalid")
            }
            Self::SourceDimensionsTooLarge => {
                formatter.write_str("game icon PNG dimensions exceed the decode bound")
            }
            Self::PngDecodeFailed => formatter.write_str("game icon PNG decode failed"),
            Self::PngOutputMismatch => {
                formatter.write_str("game icon PNG output does not match its preflight header")
            }
            Self::RgbaOutputTooLarge => formatter.write_str("decoded game icon is too large"),
            Self::AllocationFailed(context) => {
                write!(formatter, "allocate bounded game icon {context}")
            }
        }
    }
}

impl Error for GameIconError {}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct GameIconId {
    owner: ProbeSessionOwner,
    item: GameItemIdentity,
}

impl GameIconId {
    pub fn new(owner: ProbeSessionOwner, item: GameItemIdentity) -> Result<Self, GameIconError> {
        if let GameItemIdentity::Candidate(candidate) = &item {
            if candidate.token().owner != owner {
                return Err(GameIconError::CandidateOwnerMismatch);
            }
        }
        Ok(Self { owner, item })
    }

    pub const fn owner(&self) -> ProbeSessionOwner {
        self.owner
    }

    pub const fn item(&self) -> &GameItemIdentity {
        &self.item
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GameIconLoadState {
    Missing,
    Loading,
    Ready,
    Failed,
}

#[derive(Clone, PartialEq, Eq)]
pub struct GameIconSource(GameIconSourceKind);

#[derive(Debug, Clone, PartialEq, Eq)]
enum GameIconSourceKind {
    PngDataUrl(Arc<str>),
    FirstPartyAssetPath(Arc<str>),
    Missing,
}

impl fmt::Debug for GameIconSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match &self.0 {
            GameIconSourceKind::PngDataUrl(_) => "png_data_url",
            GameIconSourceKind::FirstPartyAssetPath(_) => "first_party_asset",
            GameIconSourceKind::Missing => "missing",
        };
        formatter
            .debug_struct("GameIconSource")
            .field("kind", &kind)
            .field("fingerprint", &self.fingerprint())
            .finish()
    }
}

impl GameIconSource {
    pub fn png_data_url(value: String) -> Result<Self, GameIconError> {
        let Some(payload) = value.strip_prefix(PNG_DATA_URL_PREFIX) else {
            return Err(GameIconError::UnsupportedPngDataUrl);
        };
        if value.len() > MAX_GAME_ICON_DATA_URL_BYTES || payload.len() > MAX_GAME_ICON_BASE64_BYTES
        {
            return Err(GameIconError::EncodedSourceTooLarge);
        }
        Ok(Self(GameIconSourceKind::PngDataUrl(value.into())))
    }

    pub fn first_party_asset_path(value: String) -> Result<Self, GameIconError> {
        if value.len() > MAX_GAME_ICON_ASSET_PATH_BYTES {
            return Err(GameIconError::AssetPathTooLarge);
        }
        let value = value.trim();
        if !matches!(
            value,
            "assets/games/league-of-legends.png" | "assets/games/osu.png"
        ) {
            return Err(GameIconError::InvalidAssetPath);
        }
        Ok(Self(GameIconSourceKind::FirstPartyAssetPath(value.into())))
    }

    pub const fn missing() -> Self {
        Self(GameIconSourceKind::Missing)
    }

    pub fn as_png_data_url(&self) -> Option<&str> {
        match &self.0 {
            GameIconSourceKind::PngDataUrl(value) => Some(value),
            GameIconSourceKind::FirstPartyAssetPath(_) | GameIconSourceKind::Missing => None,
        }
    }

    pub fn as_first_party_asset_path(&self) -> Option<&str> {
        match &self.0 {
            GameIconSourceKind::FirstPartyAssetPath(value) => Some(value),
            GameIconSourceKind::PngDataUrl(_) | GameIconSourceKind::Missing => None,
        }
    }

    pub const fn is_missing(&self) -> bool {
        matches!(self.0, GameIconSourceKind::Missing)
    }

    /// Stable provenance for cache work. Item identity alone is insufficient:
    /// a saved game may keep its identity while its icon bytes change.
    pub fn fingerprint(&self) -> GameIconSourceFingerprint {
        let mut digest = Sha256::new();
        match &self.0 {
            GameIconSourceKind::PngDataUrl(value) => {
                digest.update([0]);
                digest.update((value.len() as u64).to_le_bytes());
                digest.update(value.as_bytes());
            }
            GameIconSourceKind::FirstPartyAssetPath(value) => {
                digest.update([1]);
                digest.update((value.len() as u64).to_le_bytes());
                digest.update(value.as_bytes());
            }
            GameIconSourceKind::Missing => digest.update([2]),
        }
        GameIconSourceFingerprint(digest.finalize().into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GameIconSourceFingerprint([u8; 32]);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameIconCacheError {
    ZeroGeneration,
    GenerationExhausted,
    TicketExhausted,
    OwnerMismatch,
    ManifestTooLarge,
    DuplicateIdentity,
    StaleManifest,
    InvalidViewport,
    CompletionMismatch,
    InvalidDecodedImage,
    AllocationFailed(&'static str),
}

impl fmt::Display for GameIconCacheError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroGeneration => formatter.write_str("game icon generation must be nonzero"),
            Self::GenerationExhausted => formatter.write_str("game icon generation is exhausted"),
            Self::TicketExhausted => formatter.write_str("game icon work ticket is exhausted"),
            Self::OwnerMismatch => formatter.write_str("game icon owner does not match"),
            Self::ManifestTooLarge => formatter.write_str("game icon manifest is too large"),
            Self::DuplicateIdentity => {
                formatter.write_str("game icon manifest contains duplicates")
            }
            Self::StaleManifest => formatter.write_str("game icon manifest is stale"),
            Self::InvalidViewport => formatter.write_str("game icon viewport is invalid"),
            Self::CompletionMismatch => formatter.write_str("game icon completion is stale"),
            Self::InvalidDecodedImage => formatter.write_str("decoded game icon is invalid"),
            Self::AllocationFailed(field) => write!(formatter, "allocate bounded {field}"),
        }
    }
}

impl Error for GameIconCacheError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GameIconManifestGeneration(u64);

impl GameIconManifestGeneration {
    pub const fn new(value: u64) -> Result<Self, GameIconCacheError> {
        if value == 0 {
            Err(GameIconCacheError::ZeroGeneration)
        } else {
            Ok(Self(value))
        }
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub const fn checked_next(self) -> Result<Self, GameIconCacheError> {
        match self.0.checked_add(1) {
            Some(next) => Ok(Self(next)),
            None => Err(GameIconCacheError::GenerationExhausted),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameIconManifestEntry {
    id: GameIconId,
    source: GameIconSource,
    source_fingerprint: GameIconSourceFingerprint,
}

impl GameIconManifestEntry {
    // The Games controller becomes the only production manifest builder in
    // the next Task 8 slice. Keeping this crate-private prevents arbitrary
    // frontend-supplied id/source pairs from becoming cache authority.
    #[allow(dead_code)]
    pub(crate) fn new(id: GameIconId, source: GameIconSource) -> Self {
        let source_fingerprint = source.fingerprint();
        Self {
            id,
            source,
            source_fingerprint,
        }
    }

    pub const fn id(&self) -> &GameIconId {
        &self.id
    }

    pub const fn source(&self) -> &GameIconSource {
        &self.source
    }

    pub const fn source_fingerprint(&self) -> GameIconSourceFingerprint {
        self.source_fingerprint
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameIconManifest {
    owner: ProbeSessionOwner,
    generation: GameIconManifestGeneration,
    entries: Vec<GameIconManifestEntry>,
}

impl GameIconManifest {
    #[allow(dead_code)]
    pub(crate) fn new(
        owner: ProbeSessionOwner,
        generation: GameIconManifestGeneration,
        entries: Vec<GameIconManifestEntry>,
    ) -> Result<Self, GameIconCacheError> {
        if entries.len() > MAX_GAME_ICON_MANIFEST_ENTRIES {
            return Err(GameIconCacheError::ManifestTooLarge);
        }
        for (index, entry) in entries.iter().enumerate() {
            if entry.id.owner() != owner {
                return Err(GameIconCacheError::OwnerMismatch);
            }
            if entries[..index].iter().any(|prior| prior.id == entry.id) {
                return Err(GameIconCacheError::DuplicateIdentity);
            }
        }
        Ok(Self {
            owner,
            generation,
            entries,
        })
    }

    pub const fn owner(&self) -> ProbeSessionOwner {
        self.owner
    }

    pub const fn generation(&self) -> GameIconManifestGeneration {
        self.generation
    }

    pub fn entries(&self) -> &[GameIconManifestEntry] {
        &self.entries
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct GameIconTicket(u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameIconWork {
    manifest_generation: GameIconManifestGeneration,
    ticket: GameIconTicket,
    id: GameIconId,
    source_fingerprint: GameIconSourceFingerprint,
    source: GameIconSource,
}

impl GameIconWork {
    pub const fn id(&self) -> &GameIconId {
        &self.id
    }

    pub const fn source(&self) -> &GameIconSource {
        &self.source
    }

    pub const fn source_fingerprint(&self) -> GameIconSourceFingerprint {
        self.source_fingerprint
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameIconCompletionOutcome {
    Ready,
    Failed,
    Ignored,
}

#[derive(Debug)]
pub struct GameIconCacheUpdate {
    pub queued: Vec<GameIconWork>,
    pub canceled: Vec<GameIconWork>,
    pub released: usize,
    pub admission_error: Option<GameIconCacheError>,
}

impl GameIconCacheUpdate {
    fn empty() -> Self {
        Self {
            queued: Vec::new(),
            canceled: Vec::new(),
            released: 0,
            admission_error: None,
        }
    }
}

#[derive(Debug)]
pub struct GameIconCompletion {
    pub outcome: GameIconCompletionOutcome,
    pub update: GameIconCacheUpdate,
}

enum CachedIconState<H> {
    Missing,
    Loading(GameIconWork),
    Ready { handle: H, rgba_bytes: usize },
    Failed,
}

struct CachedIconEntry<H> {
    descriptor: GameIconManifestEntry,
    state: CachedIconState<H>,
}

/// UI-thread-owned bounded cache. Decoder workers receive move-free work
/// tokens; the platform handle constructor runs only after the final exact
/// token check on the owning thread.
pub struct GameIconCache<H> {
    owner: ProbeSessionOwner,
    generation: Option<GameIconManifestGeneration>,
    entries: Vec<CachedIconEntry<H>>,
    viewport: Vec<GameIconId>,
    canceled: Vec<GameIconWork>,
    next_ticket: u64,
}

impl<H> GameIconCache<H> {
    pub const fn new(owner: ProbeSessionOwner) -> Self {
        Self {
            owner,
            generation: None,
            entries: Vec::new(),
            viewport: Vec::new(),
            canceled: Vec::new(),
            next_ticket: 0,
        }
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn set_next_ticket_for_test(&mut self, value: u64) {
        self.next_ticket = value;
    }

    pub fn sync_manifest(
        &mut self,
        manifest: GameIconManifest,
    ) -> Result<GameIconCacheUpdate, GameIconCacheError> {
        if manifest.owner != self.owner {
            return Err(GameIconCacheError::OwnerMismatch);
        }
        if self
            .generation
            .is_some_and(|generation| manifest.generation <= generation)
        {
            return Err(GameIconCacheError::StaleManifest);
        }

        let mut next_entries = Vec::new();
        next_entries
            .try_reserve_exact(manifest.entries.len())
            .map_err(|_| GameIconCacheError::AllocationFailed("game icon manifest entries"))?;
        let mut update = GameIconCacheUpdate::empty();
        let cancellation_count = self
            .entries
            .iter()
            .filter(|entry| matches!(entry.state, CachedIconState::Loading(_)))
            .count();
        self.canceled
            .try_reserve_exact(cancellation_count)
            .map_err(|_| GameIconCacheError::AllocationFailed("canceled game icon work"))?;
        update
            .canceled
            .try_reserve_exact(cancellation_count)
            .map_err(|_| GameIconCacheError::AllocationFailed("game icon cancellation update"))?;
        let mut old_entries = std::mem::take(&mut self.entries);

        for descriptor in manifest.entries {
            let retained = old_entries
                .iter()
                .position(|entry| {
                    entry.descriptor.id == descriptor.id
                        && entry.descriptor.source_fingerprint == descriptor.source_fingerprint
                })
                .map(|index| old_entries.swap_remove(index));
            let state = match retained.map(|entry| entry.state) {
                Some(CachedIconState::Ready { handle, rgba_bytes }) => {
                    CachedIconState::Ready { handle, rgba_bytes }
                }
                Some(CachedIconState::Failed) => CachedIconState::Failed,
                Some(CachedIconState::Loading(work)) => {
                    self.remember_canceled(work, &mut update)?;
                    CachedIconState::Missing
                }
                Some(CachedIconState::Missing) | None => CachedIconState::Missing,
            };
            next_entries.push(CachedIconEntry { descriptor, state });
        }
        for entry in old_entries {
            match entry.state {
                CachedIconState::Loading(work) => {
                    self.remember_canceled(work, &mut update)?;
                }
                CachedIconState::Ready { .. } => update.released += 1,
                CachedIconState::Missing | CachedIconState::Failed => {}
            }
        }

        self.entries = next_entries;
        self.generation = Some(manifest.generation);
        self.viewport
            .retain(|id| self.entries.iter().any(|entry| &entry.descriptor.id == id));
        self.admit(&mut update);
        Ok(update)
    }

    pub fn set_viewport(
        &mut self,
        ids: &[GameIconId],
    ) -> Result<GameIconCacheUpdate, GameIconCacheError> {
        if ids.len() > MAX_GAME_ICON_MANIFEST_ENTRIES || self.generation.is_none() {
            return Err(GameIconCacheError::InvalidViewport);
        }
        for (index, id) in ids.iter().enumerate() {
            if id.owner() != self.owner
                || ids[..index].contains(id)
                || !self.entries.iter().any(|entry| &entry.descriptor.id == id)
            {
                return Err(GameIconCacheError::InvalidViewport);
            }
        }

        let mut next_viewport = Vec::new();
        next_viewport
            .try_reserve_exact(ids.len())
            .map_err(|_| GameIconCacheError::AllocationFailed("game icon viewport"))?;
        next_viewport.extend_from_slice(ids);
        let mut update = GameIconCacheUpdate::empty();
        let cancellation_count = self
            .entries
            .iter()
            .filter(|entry| {
                !next_viewport.contains(&entry.descriptor.id)
                    && matches!(entry.state, CachedIconState::Loading(_))
            })
            .count();
        self.canceled
            .try_reserve_exact(cancellation_count)
            .map_err(|_| GameIconCacheError::AllocationFailed("canceled game icon work"))?;
        update
            .canceled
            .try_reserve_exact(cancellation_count)
            .map_err(|_| GameIconCacheError::AllocationFailed("game icon cancellation update"))?;
        for index in 0..self.entries.len() {
            if next_viewport.contains(&self.entries[index].descriptor.id) {
                continue;
            }
            let state = std::mem::replace(&mut self.entries[index].state, CachedIconState::Missing);
            match state {
                CachedIconState::Loading(work) => self.remember_canceled(work, &mut update)?,
                CachedIconState::Ready { .. } => update.released += 1,
                CachedIconState::Missing | CachedIconState::Failed => {}
            }
        }
        self.viewport = next_viewport;
        self.admit(&mut update);
        Ok(update)
    }

    pub fn acknowledge_canceled(
        &mut self,
        work: &GameIconWork,
    ) -> Result<GameIconCacheUpdate, GameIconCacheError> {
        let Some(index) = self.canceled.iter().position(|candidate| candidate == work) else {
            return Err(GameIconCacheError::CompletionMismatch);
        };
        self.canceled.swap_remove(index);
        let mut update = GameIconCacheUpdate::empty();
        self.admit(&mut update);
        Ok(update)
    }

    pub fn complete_failed(
        &mut self,
        work: &GameIconWork,
    ) -> Result<GameIconCompletion, GameIconCacheError> {
        if let Some(index) = self.canceled.iter().position(|candidate| candidate == work) {
            self.canceled.swap_remove(index);
            let mut update = GameIconCacheUpdate::empty();
            self.admit(&mut update);
            return Ok(GameIconCompletion {
                outcome: GameIconCompletionOutcome::Ignored,
                update,
            });
        }
        let index = self.active_work_index(work)?;
        self.entries[index].state = CachedIconState::Failed;
        let mut update = GameIconCacheUpdate::empty();
        self.admit(&mut update);
        Ok(GameIconCompletion {
            outcome: GameIconCompletionOutcome::Failed,
            update,
        })
    }

    pub fn complete_decoded(
        &mut self,
        work: &GameIconWork,
        decoded: DecodedGameIcon,
        make_handle: impl FnOnce(DecodedGameIcon) -> Result<H, String>,
    ) -> Result<GameIconCompletion, GameIconCacheError> {
        if let Some(index) = self.canceled.iter().position(|candidate| candidate == work) {
            self.canceled.swap_remove(index);
            drop(decoded);
            let mut update = GameIconCacheUpdate::empty();
            self.admit(&mut update);
            return Ok(GameIconCompletion {
                outcome: GameIconCompletionOutcome::Ignored,
                update,
            });
        }
        let index = self.active_work_index(work)?;
        let rgba_bytes = match validate_decoded(&decoded) {
            Ok(bytes) => bytes,
            Err(_) => {
                self.entries[index].state = CachedIconState::Failed;
                let mut update = GameIconCacheUpdate::empty();
                self.admit(&mut update);
                return Ok(GameIconCompletion {
                    outcome: GameIconCompletionOutcome::Failed,
                    update,
                });
            }
        };
        match make_handle(decoded) {
            Ok(handle) => {
                self.entries[index].state = CachedIconState::Ready { handle, rgba_bytes };
                Ok(GameIconCompletion {
                    outcome: GameIconCompletionOutcome::Ready,
                    update: GameIconCacheUpdate::empty(),
                })
            }
            Err(_) => {
                self.entries[index].state = CachedIconState::Failed;
                let mut update = GameIconCacheUpdate::empty();
                self.admit(&mut update);
                Ok(GameIconCompletion {
                    outcome: GameIconCompletionOutcome::Failed,
                    update,
                })
            }
        }
    }

    pub fn detach(&mut self) -> Result<GameIconCacheUpdate, GameIconCacheError> {
        let mut update = GameIconCacheUpdate::empty();
        let cancellation_count = self
            .entries
            .iter()
            .filter(|entry| matches!(entry.state, CachedIconState::Loading(_)))
            .count();
        self.canceled
            .try_reserve_exact(cancellation_count)
            .map_err(|_| GameIconCacheError::AllocationFailed("canceled game icon work"))?;
        update
            .canceled
            .try_reserve_exact(cancellation_count)
            .map_err(|_| GameIconCacheError::AllocationFailed("game icon cancellation update"))?;
        for index in 0..self.entries.len() {
            let state = std::mem::replace(&mut self.entries[index].state, CachedIconState::Missing);
            match state {
                CachedIconState::Loading(work) => self.remember_canceled(work, &mut update)?,
                CachedIconState::Ready { .. } => update.released += 1,
                CachedIconState::Missing | CachedIconState::Failed => {}
            }
        }
        self.entries.clear();
        self.viewport.clear();
        self.generation = None;
        Ok(update)
    }

    pub fn load_state(&self, id: &GameIconId) -> GameIconLoadState {
        self.entries
            .iter()
            .find(|entry| &entry.descriptor.id == id)
            .map_or(GameIconLoadState::Missing, |entry| match entry.state {
                CachedIconState::Missing => GameIconLoadState::Missing,
                CachedIconState::Loading(_) => GameIconLoadState::Loading,
                CachedIconState::Ready { .. } => GameIconLoadState::Ready,
                CachedIconState::Failed => GameIconLoadState::Failed,
            })
    }

    pub fn issued_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| matches!(entry.state, CachedIconState::Loading(_)))
            .count()
    }

    pub fn retained_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| matches!(entry.state, CachedIconState::Ready { .. }))
            .count()
    }

    pub fn ownership_count(&self) -> usize {
        self.issued_count() + self.retained_count() + self.canceled.len()
    }

    pub fn owned_rgba_bytes(&self) -> usize {
        self.entries
            .iter()
            .filter_map(|entry| match entry.state {
                CachedIconState::Ready { rgba_bytes, .. } => Some(rgba_bytes),
                _ => None,
            })
            .sum()
    }

    fn active_work_index(&self, work: &GameIconWork) -> Result<usize, GameIconCacheError> {
        self.entries
            .iter()
            .position(
                |entry| matches!(&entry.state, CachedIconState::Loading(active) if active == work),
            )
            .ok_or(GameIconCacheError::CompletionMismatch)
    }

    fn remember_canceled(
        &mut self,
        work: GameIconWork,
        update: &mut GameIconCacheUpdate,
    ) -> Result<(), GameIconCacheError> {
        self.canceled
            .try_reserve(1)
            .map_err(|_| GameIconCacheError::AllocationFailed("canceled game icon work"))?;
        update
            .canceled
            .try_reserve(1)
            .map_err(|_| GameIconCacheError::AllocationFailed("game icon cancellation update"))?;
        self.canceled.push(work.clone());
        update.canceled.push(work);
        Ok(())
    }

    fn admit(&mut self, update: &mut GameIconCacheUpdate) {
        let Some(manifest_generation) = self.generation else {
            return;
        };
        let available = MAX_GAME_ICON_SLOTS.saturating_sub(self.ownership_count());
        let candidate_count = self
            .viewport
            .iter()
            .filter(|id| {
                self.entries.iter().any(|entry| {
                    &entry.descriptor.id == *id
                        && matches!(entry.state, CachedIconState::Missing)
                        && !entry.descriptor.source.is_missing()
                })
            })
            .take(available)
            .count();
        let ticket_range = u64::try_from(candidate_count)
            .ok()
            .and_then(|count| self.next_ticket.checked_add(count));
        if ticket_range.is_none() {
            update.admission_error = Some(GameIconCacheError::TicketExhausted);
            self.fail_unadmitted_viewport_entries();
            return;
        }
        if update.queued.try_reserve_exact(candidate_count).is_err() {
            update.admission_error = Some(GameIconCacheError::AllocationFailed(
                "game icon work update",
            ));
            return;
        }

        for viewport_index in 0..self.viewport.len() {
            if self.ownership_count() == MAX_GAME_ICON_SLOTS {
                break;
            }
            let id = self.viewport[viewport_index].clone();
            let Some(index) = self
                .entries
                .iter()
                .position(|entry| entry.descriptor.id == id)
            else {
                continue;
            };
            if !matches!(self.entries[index].state, CachedIconState::Missing)
                || self.entries[index].descriptor.source.is_missing()
            {
                continue;
            }
            let next_ticket = self
                .next_ticket
                .checked_add(1)
                .expect("ticket range was preflighted");
            let work = GameIconWork {
                manifest_generation,
                ticket: GameIconTicket(next_ticket),
                id: self.entries[index].descriptor.id.clone(),
                source_fingerprint: self.entries[index].descriptor.source_fingerprint,
                source: self.entries[index].descriptor.source.clone(),
            };
            self.next_ticket = next_ticket;
            self.entries[index].state = CachedIconState::Loading(work.clone());
            update.queued.push(work);
        }
        debug_assert!(self.ownership_count() <= MAX_GAME_ICON_SLOTS);
        debug_assert!(self.owned_rgba_bytes() <= MAX_GAME_ICON_DECODED_OWNERSHIP_BYTES);
    }

    fn fail_unadmitted_viewport_entries(&mut self) {
        for id in &self.viewport {
            if let Some(entry) = self
                .entries
                .iter_mut()
                .find(|entry| &entry.descriptor.id == id)
            {
                if matches!(entry.state, CachedIconState::Missing)
                    && !entry.descriptor.source.is_missing()
                {
                    entry.state = CachedIconState::Failed;
                }
            }
        }
    }
}

fn validate_decoded(decoded: &DecodedGameIcon) -> Result<usize, GameIconCacheError> {
    if decoded.width == 0
        || decoded.height == 0
        || decoded.width > MAX_GAME_ICON_DIMENSION
        || decoded.height > MAX_GAME_ICON_DIMENSION
    {
        return Err(GameIconCacheError::InvalidDecodedImage);
    }
    let expected = usize::try_from(decoded.width)
        .ok()
        .and_then(|width| {
            usize::try_from(decoded.height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(GameIconCacheError::InvalidDecodedImage)?;
    if decoded.rgba.len() != expected || expected > MAX_GAME_ICON_RGBA_BYTES {
        return Err(GameIconCacheError::InvalidDecodedImage);
    }
    Ok(expected)
}

#[derive(Debug, PartialEq, Eq)]
pub struct DecodedGameIcon {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl DecodedGameIcon {
    pub const fn width(&self) -> u32 {
        self.width
    }

    pub const fn height(&self) -> u32 {
        self.height
    }

    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }

    pub fn into_rgba(self) -> Vec<u8> {
        self.rgba
    }
}

pub fn decode_game_icon_source(source: &GameIconSource) -> Result<DecodedGameIcon, GameIconError> {
    decode_game_icon_source_with(source, &mut SystemIconAllocator)
}

trait IconAllocator {
    fn empty_with_capacity(
        &mut self,
        capacity: usize,
        context: &'static str,
    ) -> Result<Vec<u8>, GameIconError>;

    fn zeroed(&mut self, length: usize, context: &'static str) -> Result<Vec<u8>, GameIconError>;
}

struct SystemIconAllocator;

impl IconAllocator for SystemIconAllocator {
    fn empty_with_capacity(
        &mut self,
        capacity: usize,
        context: &'static str,
    ) -> Result<Vec<u8>, GameIconError> {
        let mut buffer = Vec::new();
        buffer
            .try_reserve_exact(capacity)
            .map_err(|_| GameIconError::AllocationFailed(context))?;
        Ok(buffer)
    }

    fn zeroed(&mut self, length: usize, context: &'static str) -> Result<Vec<u8>, GameIconError> {
        let mut buffer = self.empty_with_capacity(length, context)?;
        buffer.resize(length, 0);
        Ok(buffer)
    }
}

fn decode_game_icon_source_with(
    source: &GameIconSource,
    allocator: &mut impl IconAllocator,
) -> Result<DecodedGameIcon, GameIconError> {
    let GameIconSourceKind::PngDataUrl(data_url) = &source.0 else {
        return Err(match &source.0 {
            GameIconSourceKind::FirstPartyAssetPath(_) => GameIconError::AssetReadDeferred,
            GameIconSourceKind::Missing => GameIconError::MissingSource,
            GameIconSourceKind::PngDataUrl(_) => unreachable!(),
        });
    };
    let payload = data_url
        .strip_prefix(PNG_DATA_URL_PREFIX)
        .ok_or(GameIconError::UnsupportedPngDataUrl)?;
    if payload.len() > MAX_GAME_ICON_BASE64_BYTES {
        return Err(GameIconError::EncodedSourceTooLarge);
    }

    let mut encoded_png = allocator.empty_with_capacity(
        MAX_BASE64_DECODE_BYTES.min(payload.len().div_ceil(4).saturating_mul(3)),
        "encoded PNG",
    )?;
    STANDARD
        .decode_vec(payload, &mut encoded_png)
        .map_err(|_| GameIconError::InvalidBase64)?;
    if encoded_png.len() > MAX_GAME_ICON_ENCODED_PNG_BYTES {
        return Err(GameIconError::EncodedPngTooLarge);
    }

    let (source_width, source_height) = inspect_png_header(&encoded_png)?;
    let mut decoder = png::Decoder::new_with_limits(
        Cursor::new(encoded_png.as_slice()),
        png::Limits {
            bytes: PNG_DECODER_ALLOCATION_BYTES,
        },
    );
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder
        .read_info()
        .map_err(|_| GameIconError::PngDecodeFailed)?;
    let decoded_length = reader.output_buffer_size();
    if decoded_length > MAX_SOURCE_RGBA_BYTES {
        return Err(GameIconError::SourceDimensionsTooLarge);
    }
    let mut decoded = allocator.zeroed(decoded_length, "decoder output")?;
    let output = reader
        .next_frame(&mut decoded)
        .map_err(|_| GameIconError::PngDecodeFailed)?;
    if output.width != source_width
        || output.height != source_height
        || output.buffer_size() > decoded.len()
    {
        return Err(GameIconError::PngOutputMismatch);
    }
    decoded.truncate(output.buffer_size());

    let (width, height) = resized_dimensions(source_width, source_height)?;
    let rgba_length = rgba_length(width, height)?;
    let mut rgba = allocator.zeroed(rgba_length, "RGBA output")?;
    copy_resized_rgba(
        &decoded,
        output.color_type,
        source_width,
        source_height,
        width,
        height,
        &mut rgba,
    )?;
    Ok(DecodedGameIcon {
        width,
        height,
        rgba,
    })
}

fn inspect_png_header(encoded_png: &[u8]) -> Result<(u32, u32), GameIconError> {
    if encoded_png.len() < PNG_HEADER_PREFLIGHT_BYTES
        || encoded_png.get(..8) != Some(PNG_SIGNATURE.as_slice())
        || encoded_png.get(12..16) != Some(b"IHDR".as_slice())
    {
        return Err(GameIconError::InvalidPngHeader);
    }
    let chunk_length = u32::from_be_bytes(
        encoded_png[8..12]
            .try_into()
            .map_err(|_| GameIconError::InvalidPngHeader)?,
    );
    if chunk_length != PNG_IHDR_LENGTH {
        return Err(GameIconError::InvalidPngHeader);
    }
    let width = u32::from_be_bytes(
        encoded_png[16..20]
            .try_into()
            .map_err(|_| GameIconError::InvalidPngHeader)?,
    );
    let height = u32::from_be_bytes(
        encoded_png[20..24]
            .try_into()
            .map_err(|_| GameIconError::InvalidPngHeader)?,
    );
    if width == 0 || height == 0 {
        return Err(GameIconError::InvalidSourceDimensions);
    }
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or(GameIconError::SourceDimensionsTooLarge)?;
    if width > MAX_SOURCE_ICON_DIMENSION
        || height > MAX_SOURCE_ICON_DIMENSION
        || pixels > MAX_SOURCE_ICON_PIXELS
    {
        return Err(GameIconError::SourceDimensionsTooLarge);
    }
    Ok((width, height))
}

fn resized_dimensions(source_width: u32, source_height: u32) -> Result<(u32, u32), GameIconError> {
    if source_width <= MAX_GAME_ICON_DIMENSION && source_height <= MAX_GAME_ICON_DIMENSION {
        return Ok((source_width, source_height));
    }
    let (width, height) = if source_width >= source_height {
        let height = (u64::from(source_height) * u64::from(MAX_GAME_ICON_DIMENSION)
            / u64::from(source_width))
        .max(1);
        (
            MAX_GAME_ICON_DIMENSION,
            u32::try_from(height).map_err(|_| GameIconError::RgbaOutputTooLarge)?,
        )
    } else {
        let width = (u64::from(source_width) * u64::from(MAX_GAME_ICON_DIMENSION)
            / u64::from(source_height))
        .max(1);
        (
            u32::try_from(width).map_err(|_| GameIconError::RgbaOutputTooLarge)?,
            MAX_GAME_ICON_DIMENSION,
        )
    };
    Ok((width, height))
}

fn rgba_length(width: u32, height: u32) -> Result<usize, GameIconError> {
    let length = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or(GameIconError::RgbaOutputTooLarge)?;
    if length > MAX_GAME_ICON_RGBA_BYTES {
        return Err(GameIconError::RgbaOutputTooLarge);
    }
    Ok(length)
}

#[allow(clippy::too_many_arguments)]
fn copy_resized_rgba(
    decoded: &[u8],
    color_type: png::ColorType,
    source_width: u32,
    source_height: u32,
    target_width: u32,
    target_height: u32,
    target: &mut [u8],
) -> Result<(), GameIconError> {
    let channels = match color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::Rgb => 3,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => return Err(GameIconError::PngOutputMismatch),
    };
    let expected_source =
        usize::try_from(u64::from(source_width) * u64::from(source_height) * channels as u64)
            .map_err(|_| GameIconError::PngOutputMismatch)?;
    if decoded.len() != expected_source || target.len() != rgba_length(target_width, target_height)?
    {
        return Err(GameIconError::PngOutputMismatch);
    }

    for target_y in 0..target_height {
        let source_y = u64::from(target_y) * u64::from(source_height) / u64::from(target_height);
        for target_x in 0..target_width {
            let source_x = u64::from(target_x) * u64::from(source_width) / u64::from(target_width);
            let source_pixel = usize::try_from(source_y * u64::from(source_width) + source_x)
                .map_err(|_| GameIconError::PngOutputMismatch)?;
            let source_offset = source_pixel
                .checked_mul(channels)
                .ok_or(GameIconError::PngOutputMismatch)?;
            let target_pixel = usize::try_from(
                u64::from(target_y) * u64::from(target_width) + u64::from(target_x),
            )
            .map_err(|_| GameIconError::PngOutputMismatch)?;
            let target_offset = target_pixel
                .checked_mul(4)
                .ok_or(GameIconError::PngOutputMismatch)?;
            let (red, green, blue, alpha) = match color_type {
                png::ColorType::Grayscale => {
                    let gray = decoded[source_offset];
                    (gray, gray, gray, 255)
                }
                png::ColorType::Rgb => (
                    decoded[source_offset],
                    decoded[source_offset + 1],
                    decoded[source_offset + 2],
                    255,
                ),
                png::ColorType::GrayscaleAlpha => {
                    let gray = decoded[source_offset];
                    (gray, gray, gray, decoded[source_offset + 1])
                }
                png::ColorType::Rgba => (
                    decoded[source_offset],
                    decoded[source_offset + 1],
                    decoded[source_offset + 2],
                    decoded[source_offset + 3],
                ),
                png::ColorType::Indexed => return Err(GameIconError::PngOutputMismatch),
            };
            target[target_offset..target_offset + 4].copy_from_slice(&[red, green, blue, alpha]);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FailingAllocator;

    impl IconAllocator for FailingAllocator {
        fn empty_with_capacity(
            &mut self,
            _capacity: usize,
            context: &'static str,
        ) -> Result<Vec<u8>, GameIconError> {
            Err(GameIconError::AllocationFailed(context))
        }

        fn zeroed(
            &mut self,
            _length: usize,
            context: &'static str,
        ) -> Result<Vec<u8>, GameIconError> {
            Err(GameIconError::AllocationFailed(context))
        }
    }

    #[test]
    fn allocation_failure_is_reported_without_partial_output() {
        let source = GameIconSource::png_data_url(
            "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADElEQVR4nGNg+M8AAAICAQB7CYxOAAAAAElFTkSuQmCC".into(),
        )
        .unwrap();
        assert_eq!(
            decode_game_icon_source_with(&source, &mut FailingAllocator).unwrap_err(),
            GameIconError::AllocationFailed("encoded PNG")
        );
    }
}
