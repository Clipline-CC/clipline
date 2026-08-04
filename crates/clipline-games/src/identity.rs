//! Scoped game identities and catalog-owned candidate authority.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use clipline_settings::{ProbeKind, ProbeToken};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest as _, Sha256};

use crate::detection::GameWindowInfo;
use crate::discovery::{
    validate_discovery_candidates, DetectedGameCandidate, DetectedGameSource, MAX_DISCOVERED_GAMES,
    MAX_DISCOVERY_CATALOG_BYTES, MAX_DISCOVERY_TEXT_BYTES,
};

pub use clipline_settings::games::{
    BUILT_IN_GAME_IDS, CS2_ID, LEAGUE_OF_LEGENDS_ID, OSU_ID, VALORANT_ID,
};

pub const MAX_CANDIDATE_AUTHORITY_BYTES: usize = MAX_DISCOVERY_TEXT_BYTES;
pub const MAX_CANDIDATE_OPAQUE_ID_BYTES: usize = 77;
pub const MAX_CANDIDATE_CATALOG_ENTRIES: usize = MAX_DISCOVERED_GAMES;

const CANDIDATE_OPAQUE_ID_PREFIX: &str = "candidate-v2-";
const INSTALLED_AUTHORITY_DOMAIN: &[u8] = b"clipline-installed-game-authority-v1";
const WINDOW_AUTHORITY_DOMAIN: &[u8] = b"clipline-game-window-authority-v1";
const LENGTH_PREFIX_BYTES: usize = size_of::<u32>();
const OPTIONAL_TAG_BYTES: usize = 1;
const INSTALLED_AUTHORITY_FRAMING_BYTES_PER_ROW: usize = INSTALLED_AUTHORITY_DOMAIN.len()
    + (4 * LENGTH_PREFIX_BYTES)
    + 1
    + (3 * (OPTIONAL_TAG_BYTES + size_of::<u32>()))
    + size_of::<u16>();
const WINDOW_AUTHORITY_FRAMING_BYTES_PER_ROW: usize = WINDOW_AUTHORITY_DOMAIN.len()
    + LENGTH_PREFIX_BYTES
    + size_of::<u32>()
    + LENGTH_PREFIX_BYTES
    + OPTIONAL_TAG_BYTES
    + LENGTH_PREFIX_BYTES;

const _: () = {
    assert!(INSTALLED_AUTHORITY_DOMAIN.len() == 36);
    assert!(WINDOW_AUTHORITY_DOMAIN.len() == 33);
    assert!(INSTALLED_AUTHORITY_FRAMING_BYTES_PER_ROW == 70);
    assert!(WINDOW_AUTHORITY_FRAMING_BYTES_PER_ROW == 50);
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GameItemIdentityError {
    UnknownPlugin(String),
    InvalidCustomId(String),
    CandidateProbeKindMismatch {
        expected: ProbeKind,
        actual: ProbeKind,
    },
    UnsupportedCandidateProbeKind(ProbeKind),
    CandidateTokenMismatch {
        expected: ProbeToken,
        actual: ProbeToken,
    },
    InvalidCandidateCatalog(String),
    DuplicateCandidateAuthority {
        first_index: usize,
        duplicate_index: usize,
    },
    CandidateOpaqueCollision {
        first_index: usize,
        collision_index: usize,
    },
    CandidateNotInCatalog,
    InvalidCandidateOpaqueId,
    AllocationFailed(&'static str),
}

impl fmt::Display for GameItemIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownPlugin(id) => write!(formatter, "unknown built-in game plugin {id:?}"),
            Self::InvalidCustomId(message) | Self::InvalidCandidateCatalog(message) => {
                formatter.write_str(message)
            }
            Self::CandidateProbeKindMismatch { expected, actual } => write!(
                formatter,
                "candidate identity catalog requires a {expected:?} probe token, got {actual:?}"
            ),
            Self::UnsupportedCandidateProbeKind(kind) => write!(
                formatter,
                "candidate identity does not support a {kind:?} probe token"
            ),
            Self::CandidateTokenMismatch { expected, actual } => write!(
                formatter,
                "candidate identity belongs to probe {actual:?}, not current probe {expected:?}"
            ),
            Self::DuplicateCandidateAuthority {
                first_index,
                duplicate_index,
            } => write!(
                formatter,
                "candidate rows {first_index} and {duplicate_index} have duplicate authority"
            ),
            Self::CandidateOpaqueCollision {
                first_index,
                collision_index,
            } => write!(
                formatter,
                "candidate rows {first_index} and {collision_index} have colliding opaque ids"
            ),
            Self::CandidateNotInCatalog => {
                formatter.write_str("candidate identity is not a member of the current catalog")
            }
            Self::InvalidCandidateOpaqueId => {
                formatter.write_str("candidate opaque id is not canonical")
            }
            Self::AllocationFailed(context) => {
                write!(formatter, "allocate bounded game identity {context}")
            }
        }
    }
}

impl Error for GameItemIdentityError {}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct PluginGameIdentity(String);

impl PluginGameIdentity {
    pub fn new(id: &str) -> Result<Self, GameItemIdentityError> {
        built_in_id(id)
            .map(|id| Self(id.to_owned()))
            .ok_or_else(|| GameItemIdentityError::UnknownPlugin(id.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for PluginGameIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(&value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct CustomGameIdentity(String);

impl CustomGameIdentity {
    pub fn new(id: &str) -> Result<Self, GameItemIdentityError> {
        clipline_settings::games::validate_custom_game_id(id)
            .map_err(GameItemIdentityError::InvalidCustomId)?;
        Ok(Self(id.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for CustomGameIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(&value).map_err(serde::de::Error::custom)
    }
}

/// A UI-safe handle minted only while building an owning candidate catalog.
///
/// Deserialization validates the wire shape but does not grant authority.
/// Every mutation must resolve this handle through the exact catalog that owns
/// its [`ProbeToken`].
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct CandidateGameIdentity {
    token: ProbeToken,
    opaque_id: String,
}

impl CandidateGameIdentity {
    pub const fn token(&self) -> ProbeToken {
        self.token
    }

    pub fn opaque_id(&self) -> &str {
        &self.opaque_id
    }

    fn catalog_member(token: ProbeToken, opaque_id: String) -> Self {
        debug_assert!(candidate_opaque_id_is_canonical(&opaque_id));
        Self { token, opaque_id }
    }

    fn from_serialized_parts(
        token: ProbeToken,
        opaque_id: String,
    ) -> Result<Self, GameItemIdentityError> {
        validate_supported_candidate_token(token)?;
        if !candidate_opaque_id_is_canonical(&opaque_id) {
            return Err(GameItemIdentityError::InvalidCandidateOpaqueId);
        }
        Ok(Self { token, opaque_id })
    }
}

#[derive(Deserialize)]
struct CandidateGameIdentityWire {
    token: ProbeToken,
    opaque_id: String,
}

impl<'de> Deserialize<'de> for CandidateGameIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = CandidateGameIdentityWire::deserialize(deserializer)?;
        Self::from_serialized_parts(wire.token, wire.opaque_id).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum GameItemIdentity {
    Plugin(PluginGameIdentity),
    Custom(CustomGameIdentity),
    Candidate(CandidateGameIdentity),
}

#[derive(Debug)]
struct CandidateCatalogMember<T> {
    identity: CandidateGameIdentity,
    source: T,
}

#[derive(Debug)]
struct CandidateIdentityCatalog<T> {
    token: ProbeToken,
    members: Vec<CandidateCatalogMember<T>>,
    indices_by_opaque_id: BTreeMap<String, usize>,
}

impl<T> CandidateIdentityCatalog<T> {
    const fn token(&self) -> ProbeToken {
        self.token
    }

    fn len(&self) -> usize {
        self.members.len()
    }

    fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    fn identity_at(&self, index: usize) -> Option<&CandidateGameIdentity> {
        self.members.get(index).map(|member| &member.identity)
    }

    fn source_at(&self, index: usize) -> Option<&T> {
        self.members.get(index).map(|member| &member.source)
    }

    fn iter(
        &self,
    ) -> impl ExactSizeIterator<Item = (&CandidateGameIdentity, &T)> + DoubleEndedIterator {
        self.members
            .iter()
            .map(|member| (&member.identity, &member.source))
    }

    fn resolve_index(
        &self,
        identity: &CandidateGameIdentity,
    ) -> Result<usize, GameItemIdentityError> {
        if identity.token != self.token {
            return Err(GameItemIdentityError::CandidateTokenMismatch {
                expected: self.token,
                actual: identity.token,
            });
        }
        let index = self
            .indices_by_opaque_id
            .get(identity.opaque_id())
            .copied()
            .ok_or(GameItemIdentityError::CandidateNotInCatalog)?;
        if self.members.get(index).map(|member| &member.identity) != Some(identity) {
            return Err(GameItemIdentityError::CandidateNotInCatalog);
        }
        Ok(index)
    }

    fn resolve(&self, identity: &CandidateGameIdentity) -> Result<&T, GameItemIdentityError> {
        let index = self.resolve_index(identity)?;
        self.source_at(index)
            .ok_or(GameItemIdentityError::CandidateNotInCatalog)
    }
}

#[derive(Debug)]
pub struct InstalledGameIdentityCatalog {
    inner: CandidateIdentityCatalog<DetectedGameCandidate>,
}

impl InstalledGameIdentityCatalog {
    pub fn build(
        token: ProbeToken,
        sources: Vec<DetectedGameCandidate>,
    ) -> Result<Self, GameItemIdentityError> {
        validate_catalog_token(token, ProbeKind::InstalledGames)?;
        validate_discovery_candidates(&sources)
            .map_err(GameItemIdentityError::InvalidCandidateCatalog)?;
        let authority_ceiling =
            derived_authority_ceiling(sources.len(), INSTALLED_AUTHORITY_FRAMING_BYTES_PER_ROW)?;
        build_candidate_catalog(
            token,
            sources,
            authority_ceiling,
            installed_candidate_authority,
            sha256_digest,
        )
        .map(|inner| Self { inner })
    }

    pub const fn token(&self) -> ProbeToken {
        self.inner.token()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn identity_at(&self, index: usize) -> Option<&CandidateGameIdentity> {
        self.inner.identity_at(index)
    }

    pub fn source_at(&self, index: usize) -> Option<&DetectedGameCandidate> {
        self.inner.source_at(index)
    }

    pub fn iter(
        &self,
    ) -> impl ExactSizeIterator<Item = (&CandidateGameIdentity, &DetectedGameCandidate)>
           + DoubleEndedIterator {
        self.inner.iter()
    }

    pub fn resolve_index(
        &self,
        identity: &CandidateGameIdentity,
    ) -> Result<usize, GameItemIdentityError> {
        self.inner.resolve_index(identity)
    }

    pub fn resolve(
        &self,
        identity: &CandidateGameIdentity,
    ) -> Result<&DetectedGameCandidate, GameItemIdentityError> {
        self.inner.resolve(identity)
    }
}

#[derive(Debug)]
pub struct GameWindowIdentityCatalog {
    inner: CandidateIdentityCatalog<GameWindowInfo>,
}

impl GameWindowIdentityCatalog {
    pub fn build(
        token: ProbeToken,
        sources: Vec<GameWindowInfo>,
    ) -> Result<Self, GameItemIdentityError> {
        validate_catalog_token(token, ProbeKind::GameWindows)?;
        validate_window_catalog(&sources)?;
        let authority_ceiling =
            derived_authority_ceiling(sources.len(), WINDOW_AUTHORITY_FRAMING_BYTES_PER_ROW)?;
        build_candidate_catalog(
            token,
            sources,
            authority_ceiling,
            game_window_authority,
            sha256_digest,
        )
        .map(|inner| Self { inner })
    }

    pub const fn token(&self) -> ProbeToken {
        self.inner.token()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn identity_at(&self, index: usize) -> Option<&CandidateGameIdentity> {
        self.inner.identity_at(index)
    }

    pub fn source_at(&self, index: usize) -> Option<&GameWindowInfo> {
        self.inner.source_at(index)
    }

    pub fn iter(
        &self,
    ) -> impl ExactSizeIterator<Item = (&CandidateGameIdentity, &GameWindowInfo)> + DoubleEndedIterator
    {
        self.inner.iter()
    }

    pub fn resolve_index(
        &self,
        identity: &CandidateGameIdentity,
    ) -> Result<usize, GameItemIdentityError> {
        self.inner.resolve_index(identity)
    }

    pub fn resolve(
        &self,
        identity: &CandidateGameIdentity,
    ) -> Result<&GameWindowInfo, GameItemIdentityError> {
        self.inner.resolve(identity)
    }
}

fn build_candidate_catalog<T, A, D>(
    token: ProbeToken,
    sources: Vec<T>,
    maximum_authority_bytes: usize,
    authority_for: A,
    digest_for: D,
) -> Result<CandidateIdentityCatalog<T>, GameItemIdentityError>
where
    A: Fn(&T) -> Result<Vec<u8>, GameItemIdentityError>,
    D: Fn(&[u8]) -> [u8; 32],
{
    if sources.len() > MAX_CANDIDATE_CATALOG_ENTRIES {
        return Err(GameItemIdentityError::InvalidCandidateCatalog(format!(
            "candidate identity catalog has {} rows; maximum is {MAX_CANDIDATE_CATALOG_ENTRIES}",
            sources.len()
        )));
    }

    let mut members = Vec::new();
    members
        .try_reserve_exact(sources.len())
        .map_err(|_| GameItemIdentityError::AllocationFailed("catalog members"))?;
    let mut authorities = Vec::<Vec<u8>>::new();
    authorities
        .try_reserve_exact(sources.len())
        .map_err(|_| GameItemIdentityError::AllocationFailed("catalog authorities"))?;
    let mut indices_by_opaque_id = BTreeMap::<String, usize>::new();
    let mut aggregate_authority_bytes = 0_usize;

    for (index, source) in sources.into_iter().enumerate() {
        let authority = authority_for(&source)?;
        aggregate_authority_bytes = aggregate_authority_bytes
            .checked_add(authority.len())
            .ok_or_else(|| {
                GameItemIdentityError::InvalidCandidateCatalog(
                    "candidate identity authority byte count overflowed".into(),
                )
            })?;
        if aggregate_authority_bytes > maximum_authority_bytes {
            return Err(GameItemIdentityError::InvalidCandidateCatalog(format!(
                "candidate identity authority is {aggregate_authority_bytes} bytes; maximum is {maximum_authority_bytes}"
            )));
        }

        let opaque_id = opaque_id_from_digest(digest_for(&authority))?;
        if let Some(first_index) = indices_by_opaque_id.get(&opaque_id).copied() {
            if authorities.get(first_index) == Some(&authority) {
                return Err(GameItemIdentityError::DuplicateCandidateAuthority {
                    first_index,
                    duplicate_index: index,
                });
            }
            return Err(GameItemIdentityError::CandidateOpaqueCollision {
                first_index,
                collision_index: index,
            });
        }

        indices_by_opaque_id.insert(opaque_id.clone(), index);
        authorities.push(authority);
        members.push(CandidateCatalogMember {
            identity: CandidateGameIdentity::catalog_member(token, opaque_id),
            source,
        });
    }

    Ok(CandidateIdentityCatalog {
        token,
        members,
        indices_by_opaque_id,
    })
}

fn derived_authority_ceiling(
    row_count: usize,
    framing_bytes_per_row: usize,
) -> Result<usize, GameItemIdentityError> {
    let framing = framing_bytes_per_row
        .checked_mul(row_count)
        .ok_or_else(|| {
            GameItemIdentityError::InvalidCandidateCatalog(
                "candidate identity framing byte count overflowed".into(),
            )
        })?;
    MAX_DISCOVERY_CATALOG_BYTES
        .checked_add(framing)
        .ok_or_else(|| {
            GameItemIdentityError::InvalidCandidateCatalog(
                "candidate identity authority ceiling overflowed".into(),
            )
        })
}

fn validate_catalog_token(
    token: ProbeToken,
    expected: ProbeKind,
) -> Result<(), GameItemIdentityError> {
    if token.kind != expected {
        return Err(GameItemIdentityError::CandidateProbeKindMismatch {
            expected,
            actual: token.kind,
        });
    }
    Ok(())
}

fn validate_supported_candidate_token(token: ProbeToken) -> Result<(), GameItemIdentityError> {
    if !matches!(
        token.kind,
        ProbeKind::InstalledGames | ProbeKind::GameWindows
    ) {
        return Err(GameItemIdentityError::UnsupportedCandidateProbeKind(
            token.kind,
        ));
    }
    Ok(())
}

fn validate_window_catalog(sources: &[GameWindowInfo]) -> Result<(), GameItemIdentityError> {
    if sources.len() > MAX_CANDIDATE_CATALOG_ENTRIES {
        return Err(GameItemIdentityError::InvalidCandidateCatalog(format!(
            "game window catalog has {} rows; maximum is {MAX_CANDIDATE_CATALOG_ENTRIES}",
            sources.len()
        )));
    }
    let mut aggregate = 0_usize;
    for source in sources {
        for value in [
            Some(source.title.as_str()),
            Some(source.exe_name.as_str()),
            source.exe_path.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if value.len() > MAX_CANDIDATE_AUTHORITY_BYTES {
                return Err(GameItemIdentityError::InvalidCandidateCatalog(format!(
                    "game window authority field is {} bytes; maximum is {MAX_CANDIDATE_AUTHORITY_BYTES}",
                    value.len()
                )));
            }
            aggregate = aggregate.checked_add(value.len()).ok_or_else(|| {
                GameItemIdentityError::InvalidCandidateCatalog(
                    "game window catalog byte count overflowed".into(),
                )
            })?;
        }
    }
    if aggregate > MAX_DISCOVERY_CATALOG_BYTES {
        return Err(GameItemIdentityError::InvalidCandidateCatalog(format!(
            "game window catalog is {aggregate} bytes; maximum is {MAX_DISCOVERY_CATALOG_BYTES}"
        )));
    }
    Ok(())
}

fn installed_candidate_authority(
    source: &DetectedGameCandidate,
) -> Result<Vec<u8>, GameItemIdentityError> {
    let mut authority = canonical_authority(INSTALLED_AUTHORITY_DOMAIN)?;
    append_text(&mut authority, &canonical_text(&source.id_hint))?;
    append_text(&mut authority, &canonical_text(&source.name))?;
    authority.push(match source.source {
        DetectedGameSource::Steam => 0,
        DetectedGameSource::RunningWindow => 1,
        DetectedGameSource::SteamAndRunningWindow => 2,
    });
    append_optional_u32(&mut authority, source.steam_app_id)?;
    append_optional_text(
        &mut authority,
        source.install_dir.as_deref().map(canonical_path),
    )?;
    append_text(&mut authority, &canonical_text(&source.exe_name))?;
    append_optional_text(
        &mut authority,
        source.process_path.as_deref().map(canonical_path),
    )?;
    append_text(&mut authority, &canonical_text(&source.window_title))?;
    append_bytes(&mut authority, &source.confidence.to_le_bytes())?;
    Ok(authority)
}

fn game_window_authority(source: &GameWindowInfo) -> Result<Vec<u8>, GameItemIdentityError> {
    let mut authority = canonical_authority(WINDOW_AUTHORITY_DOMAIN)?;
    append_text(&mut authority, &canonical_text(&source.title))?;
    append_bytes(&mut authority, &source.process_id.to_le_bytes())?;
    append_text(&mut authority, &canonical_text(&source.exe_name))?;
    append_optional_text(
        &mut authority,
        source.exe_path.as_deref().map(canonical_path),
    )?;
    Ok(authority)
}

fn canonical_authority(domain: &[u8]) -> Result<Vec<u8>, GameItemIdentityError> {
    let mut authority = Vec::new();
    authority
        .try_reserve_exact(domain.len())
        .map_err(|_| GameItemIdentityError::AllocationFailed("canonical authority"))?;
    authority.extend_from_slice(domain);
    Ok(authority)
}

fn append_optional_text(
    authority: &mut Vec<u8>,
    value: Option<String>,
) -> Result<(), GameItemIdentityError> {
    match value {
        Some(value) if !value.is_empty() => {
            append_bytes(authority, &[1])?;
            append_text(authority, &value)
        }
        _ => append_bytes(authority, &[0]),
    }
}

fn append_optional_u32(
    authority: &mut Vec<u8>,
    value: Option<u32>,
) -> Result<(), GameItemIdentityError> {
    match value {
        Some(value) => {
            append_bytes(authority, &[1])?;
            append_bytes(authority, &value.to_le_bytes())
        }
        None => append_bytes(authority, &[0]),
    }
}

fn append_text(authority: &mut Vec<u8>, value: &str) -> Result<(), GameItemIdentityError> {
    let length = u32::try_from(value.len()).map_err(|_| {
        GameItemIdentityError::InvalidCandidateCatalog(
            "candidate authority text length overflowed".into(),
        )
    })?;
    append_bytes(authority, &length.to_le_bytes())?;
    append_bytes(authority, value.as_bytes())
}

fn append_bytes(authority: &mut Vec<u8>, value: &[u8]) -> Result<(), GameItemIdentityError> {
    authority
        .try_reserve(value.len())
        .map_err(|_| GameItemIdentityError::AllocationFailed("canonical authority bytes"))?;
    authority.extend_from_slice(value);
    Ok(())
}

fn canonical_text(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn canonical_path(value: &str) -> String {
    let mut normalized = value.trim().replace('/', "\\").to_ascii_lowercase();
    while normalized.ends_with('\\') && !normalized.ends_with(":\\") {
        normalized.pop();
    }
    normalized
}

fn sha256_digest(authority: &[u8]) -> [u8; 32] {
    Sha256::digest(authority).into()
}

fn opaque_id_from_digest(digest: [u8; 32]) -> Result<String, GameItemIdentityError> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut opaque_id = String::new();
    opaque_id
        .try_reserve_exact(MAX_CANDIDATE_OPAQUE_ID_BYTES)
        .map_err(|_| GameItemIdentityError::AllocationFailed("opaque id"))?;
    opaque_id.push_str(CANDIDATE_OPAQUE_ID_PREFIX);
    for byte in digest {
        opaque_id.push(char::from(HEX[usize::from(byte >> 4)]));
        opaque_id.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    debug_assert_eq!(opaque_id.len(), MAX_CANDIDATE_OPAQUE_ID_BYTES);
    Ok(opaque_id)
}

fn candidate_opaque_id_is_canonical(value: &str) -> bool {
    value.len() == MAX_CANDIDATE_OPAQUE_ID_BYTES
        && value
            .strip_prefix(CANDIDATE_OPAQUE_ID_PREFIX)
            .is_some_and(|digest| {
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GameIdentity {
    BuiltInPlugin(&'static str),
    Custom(String),
}

impl GameIdentity {
    pub fn built_in_plugin(id: &str) -> Option<Self> {
        built_in_id(id).map(Self::BuiltInPlugin)
    }

    pub fn custom(id: impl Into<String>) -> Self {
        Self::Custom(id.into())
    }

    pub fn id(&self) -> &str {
        match self {
            Self::BuiltInPlugin(id) => id,
            Self::Custom(id) => id,
        }
    }

    pub fn plugin_id(&self) -> Option<&'static str> {
        match self {
            Self::BuiltInPlugin(id) => Some(*id),
            Self::Custom(_) => None,
        }
    }

    pub fn is_built_in_plugin(&self, id: &str) -> bool {
        matches!(
            (self.plugin_id(), built_in_id(id)),
            (Some(actual), Some(expected)) if actual == expected
        )
    }
}

pub fn built_in_id(id: &str) -> Option<&'static str> {
    BUILT_IN_GAME_IDS
        .iter()
        .copied()
        .find(|built_in| *built_in == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_settings::{
        ProbeRequestGeneration, ProbeSessionOwner, SettingsAttachmentGeneration,
        SettingsForegroundGeneration, SettingsSessionGeneration,
    };

    fn installed_token() -> ProbeToken {
        ProbeToken {
            owner: ProbeSessionOwner::new(
                SettingsSessionGeneration::new(1),
                SettingsAttachmentGeneration::new(2),
                SettingsForegroundGeneration::new(3),
            ),
            kind: ProbeKind::InstalledGames,
            request_generation: ProbeRequestGeneration::new(4),
        }
    }

    fn candidate(name: &str) -> DetectedGameCandidate {
        DetectedGameCandidate {
            id_hint: format!("steam-{name}"),
            name: name.into(),
            source: DetectedGameSource::Steam,
            steam_app_id: None,
            install_dir: None,
            exe_name: format!("{name}.exe"),
            process_path: None,
            window_title: String::new(),
            icon: None,
            confidence: 80,
        }
    }

    #[test]
    fn distinct_authorities_with_a_forced_digest_collision_fail_closed() {
        let sources = vec![candidate("first"), candidate("second")];
        validate_discovery_candidates(&sources).unwrap();
        let error = build_candidate_catalog(
            installed_token(),
            sources,
            derived_authority_ceiling(2, INSTALLED_AUTHORITY_FRAMING_BYTES_PER_ROW).unwrap(),
            installed_candidate_authority,
            |_| [7; 32],
        )
        .unwrap_err();
        assert_eq!(
            error,
            GameItemIdentityError::CandidateOpaqueCollision {
                first_index: 0,
                collision_index: 1,
            }
        );
    }
}
