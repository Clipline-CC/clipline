use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use serde::Deserialize;

use clipline_shell::open_regular_file_nofollow;
use serde::Serialize;

use clipline_events::{ClipMarkers, GameId};

use crate::{
    inferred_clip_kind_for_path, load_marker_sidecar_with_probe, ClipGame, ClipPathIdentity,
    LegacyAudioTrackProbe, LocalClipItem, ParsedMarkerSidecar, MAX_CATALOG_STRING_BYTES,
    MAX_LOCAL_INDEX_ROWS,
};

pub const LOCAL_LIBRARY_TRUNCATED_WARNING: &str =
    "Library scan was truncated to the newest 10,000 clips.";
pub const MAX_LOCAL_SCAN_WARNINGS: usize = 256;
pub const MAX_CLIP_METADATA_BYTES: usize = 64 * 1024;
pub const MAX_SESSION_GAME_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LocalScan<T> {
    pub clips: Vec<T>,
    pub warnings: Vec<String>,
}

/// Exact shipping Tauri row shape. The native compact projection never stores
/// this type because it intentionally owns the complete bounded marker document.
#[derive(Debug, Clone, Serialize)]
pub struct CompatibilityClipInfo {
    pub path: String,
    pub name: String,
    pub title: Option<String>,
    pub kind: String,
    pub session: Option<String>,
    pub size_mb: f64,
    pub modified_unix: u64,
    pub duration_s: Option<f64>,
    pub markers: Option<ClipMarkers>,
    pub game: Option<ClipGame>,
}

/// Exact shipping Tauri scan envelope (`{ clips, warnings }`).
pub type CompatibilityLocalClipScan = LocalScan<CompatibilityClipInfo>;

/// Compact native row. It owns no per-marker, per-play, or per-track vectors.
pub type CompactLocalClip = LocalClipItem;

pub trait GameIdentityResolver {
    fn resolve(&self, game: GameId) -> Option<ClipGame>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct KnownGameIdentityResolver;

impl GameIdentityResolver for KnownGameIdentityResolver {
    fn resolve(&self, game: GameId) -> Option<ClipGame> {
        let (id, name) = match game {
            GameId::LeagueOfLegends => ("league_of_legends", "League of Legends"),
            GameId::Valorant => ("valorant", "Valorant"),
            GameId::Cs2 => ("cs2", "CS2"),
            GameId::Osu => ("osu", "osu!"),
        };
        Some(ClipGame {
            id: id.into(),
            name: name.into(),
        })
    }
}

pub struct CompatibilityClipProjection<'a> {
    probe: &'a dyn LegacyAudioTrackProbe,
    games: &'a dyn GameIdentityResolver,
}

impl<'a> CompatibilityClipProjection<'a> {
    #[must_use]
    pub const fn new(
        probe: &'a dyn LegacyAudioTrackProbe,
        games: &'a dyn GameIdentityResolver,
    ) -> Self {
        Self { probe, games }
    }
}

impl ClipProjection for CompatibilityClipProjection<'_> {
    type Output = CompatibilityClipInfo;

    fn project(&self, source: &ClipScanSource) -> ClipProjectionOutput<Self::Output> {
        let (parsed, warning) = load_projected_markers(source, self.probe);
        let duration_s = parsed
            .as_ref()
            .filter(|parsed| parsed.sidecar_bytes() != 0)
            .map(|parsed| parsed.summary().duration_s);
        let game = source
            .session_game()
            .cloned()
            .or_else(|| marker_game(parsed.as_ref(), self.games));
        let output = CompatibilityClipInfo {
            path: source.display_path().to_owned(),
            name: source.name().to_owned(),
            title: source.title().map(ToOwned::to_owned),
            kind: source.kind().to_owned(),
            session: source.session().map(ToOwned::to_owned),
            size_mb: source.size_bytes() as f64 / (1024.0 * 1024.0),
            modified_unix: source.modified_unix(),
            duration_s,
            markers: parsed.map(ParsedMarkerSidecar::into_markers),
            game,
        };
        let mut projected = ClipProjectionOutput::new(output);
        if let Some(warning) = warning {
            projected = projected.with_warning(warning);
        }
        projected
    }
}

pub struct CompactClipProjection<'a> {
    probe: &'a dyn LegacyAudioTrackProbe,
    games: &'a dyn GameIdentityResolver,
}

impl<'a> CompactClipProjection<'a> {
    #[must_use]
    pub const fn new(
        probe: &'a dyn LegacyAudioTrackProbe,
        games: &'a dyn GameIdentityResolver,
    ) -> Self {
        Self { probe, games }
    }
}

impl ClipProjection for CompactClipProjection<'_> {
    type Output = CompactLocalClip;

    fn project(&self, source: &ClipScanSource) -> ClipProjectionOutput<Self::Output> {
        let (parsed, warning) = load_projected_markers(source, self.probe);
        let marker_summary = parsed
            .as_ref()
            .map(|parsed| parsed.summary().clone())
            .unwrap_or_default();
        let duration_s = parsed
            .as_ref()
            .filter(|parsed| parsed.sidecar_bytes() != 0)
            .map(|parsed| parsed.summary().duration_s);
        let game = source
            .session_game()
            .cloned()
            .or_else(|| marker_game(parsed.as_ref(), self.games));
        let output = LocalClipItem {
            path: source.display_path().to_owned(),
            name: source.name().to_owned(),
            title: source.title().map(ToOwned::to_owned),
            kind: source.kind().to_owned(),
            session: source.session().map(ToOwned::to_owned),
            size_mb: source.size_bytes() as f64 / (1024.0 * 1024.0),
            modified_unix: source.modified_unix(),
            duration_s,
            marker_count: marker_summary.review_marker_count,
            game,
            marker_summary,
        };
        let mut projected = ClipProjectionOutput::new(output);
        if let Some(warning) = warning {
            projected = projected.with_warning(warning);
        }
        projected
    }
}

fn load_projected_markers(
    source: &ClipScanSource,
    probe: &dyn LegacyAudioTrackProbe,
) -> (Option<ParsedMarkerSidecar>, Option<String>) {
    match load_marker_sidecar_with_probe(source.canonical_path(), probe) {
        Ok(parsed) => (parsed, None),
        Err(error) => (
            None,
            Some(format!(
                "Skipped invalid marker sidecar for {:?}: {error}",
                source.display_path()
            )),
        ),
    }
}

fn marker_game(
    parsed: Option<&ParsedMarkerSidecar>,
    games: &dyn GameIdentityResolver,
) -> Option<ClipGame> {
    let game = parsed?.markers().markers.first()?.event.game_id;
    games.resolve(game)
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClipProjectionOutput<T> {
    clip: T,
    warnings: Vec<String>,
}

impl<T> ClipProjectionOutput<T> {
    #[must_use]
    pub const fn new(clip: T) -> Self {
        Self {
            clip,
            warnings: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_warning(mut self, warning: impl Into<String>) -> Self {
        self.warnings.push(warning.into());
        self
    }

    #[must_use]
    pub fn with_warnings(mut self, warnings: impl IntoIterator<Item = String>) -> Self {
        self.warnings.extend(warnings);
        self
    }
}

pub trait ClipProjection {
    type Output;

    fn project(&self, source: &ClipScanSource) -> ClipProjectionOutput<Self::Output>;
}

#[derive(Debug, Clone)]
pub struct ClipScanSource {
    display_path: String,
    canonical_path: PathBuf,
    name: String,
    title: Option<String>,
    kind: String,
    session: Option<String>,
    size_bytes: u64,
    modified_unix: u64,
    session_game: Option<ClipGame>,
}

impl ClipScanSource {
    #[must_use]
    pub fn display_path(&self) -> &str {
        &self.display_path
    }

    #[must_use]
    pub fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    #[must_use]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    #[must_use]
    pub fn session(&self) -> Option<&str> {
        self.session.as_deref()
    }

    #[must_use]
    pub const fn size_bytes(&self) -> u64 {
        self.size_bytes
    }

    #[must_use]
    pub const fn modified_unix(&self) -> u64 {
        self.modified_unix
    }

    #[must_use]
    pub const fn session_game(&self) -> Option<&ClipGame> {
        self.session_game.as_ref()
    }
}

/// Narrow synchronous enumeration seam used to prove partial-session behavior
/// without changing production filesystem semantics.
pub trait LibraryDirectoryReader: std::fmt::Debug + Send + Sync {
    fn read_dir(&self, path: &Path) -> std::io::Result<std::fs::ReadDir>;
}

#[derive(Debug, Clone, Copy, Default)]
struct SystemDirectoryReader;

impl LibraryDirectoryReader for SystemDirectoryReader {
    fn read_dir(&self, path: &Path) -> std::io::Result<std::fs::ReadDir> {
        std::fs::read_dir(path)
    }
}

#[derive(Debug, Clone)]
pub struct LocalLibraryScanner {
    display_root: PathBuf,
    canonical_root: PathBuf,
    directory_reader: Arc<dyn LibraryDirectoryReader>,
}

impl LocalLibraryScanner {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, String> {
        Self::open_with_directory_reader(root, Arc::new(SystemDirectoryReader))
    }

    pub fn open_with_directory_reader(
        root: impl AsRef<Path>,
        directory_reader: Arc<dyn LibraryDirectoryReader>,
    ) -> Result<Self, String> {
        let display_root = root.as_ref().to_path_buf();
        let canonical_root = display_root
            .canonicalize()
            .map_err(|error| error.to_string())?;
        let metadata = std::fs::metadata(&canonical_root).map_err(|error| error.to_string())?;
        if !metadata.is_dir() {
            return Err("media root is not a directory".into());
        }
        Ok(Self {
            display_root,
            canonical_root,
            directory_reader,
        })
    }

    #[must_use]
    pub fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }

    pub fn scan<P: ClipProjection>(&self, projection: &P) -> Result<LocalScan<P::Output>, String> {
        let mut warnings = WarningSink::default();
        let mut candidates = CandidateSet::default();
        self.scan_root(&mut candidates, &mut warnings)?;
        if candidates.truncated() {
            warnings.push_required(LOCAL_LIBRARY_TRUNCATED_WARNING.to_string());
        }

        let ordered = candidates.into_ordered();
        let mut clips = Vec::with_capacity(ordered.len());
        for candidate in ordered {
            let source = self.build_source(candidate, &mut warnings);
            let projected = projection.project(&source);
            for warning in projected.warnings {
                warnings.push(warning);
            }
            clips.push(projected.clip);
        }
        Ok(LocalScan {
            clips,
            warnings: warnings.finish(),
        })
    }

    fn scan_root(
        &self,
        candidates: &mut CandidateSet,
        warnings: &mut WarningSink,
    ) -> Result<(), String> {
        let entries = self
            .directory_reader
            .read_dir(&self.display_root)
            .map_err(|error| error.to_string())?;
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    warnings.push(format!("Skipped an unreadable Library entry: {error}"));
                    continue;
                }
            };
            let display_name = entry.file_name().to_string_lossy().into_owned();
            let metadata = match std::fs::symlink_metadata(entry.path()) {
                Ok(metadata) => metadata,
                Err(error) => {
                    warnings.push(format!(
                        "Skipped Library entry \"{display_name}\" because its metadata is unavailable: {error}"
                    ));
                    continue;
                }
            };
            if metadata_is_link_or_reparse(&metadata) {
                warnings.push(format!(
                    "Skipped Library entry \"{display_name}\" because it is a link or reparse point"
                ));
                continue;
            }
            if metadata.is_file() {
                self.consider_file(entry.path(), None, metadata, candidates, warnings);
            } else if metadata.is_dir() {
                self.scan_session(entry.path(), display_name, candidates, warnings);
            }
        }
        Ok(())
    }

    fn scan_session(
        &self,
        directory: PathBuf,
        session: String,
        candidates: &mut CandidateSet,
        warnings: &mut WarningSink,
    ) {
        let canonical = match directory.canonicalize() {
            Ok(canonical) if canonical.parent() == Some(self.canonical_root.as_path()) => canonical,
            Ok(_) => {
                warnings.push(format!(
                    "Skipped Library session \"{session}\" because it escaped the media root"
                ));
                return;
            }
            Err(error) => {
                warnings.push(format!(
                    "Skipped Library session \"{session}\" because it could not be read: {error}"
                ));
                return;
            }
        };
        let session_game = read_session_game(&canonical, warnings);
        let entries = match self.directory_reader.read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                warnings.push(format!(
                    "Skipped Library session \"{session}\" because it could not be read: {error}"
                ));
                return;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    warnings.push(format!(
                        "Skipped an unreadable Library entry in session \"{session}\": {error}"
                    ));
                    continue;
                }
            };
            let metadata = match std::fs::symlink_metadata(entry.path()) {
                Ok(metadata) => metadata,
                Err(error) => {
                    warnings.push(format!(
                        "Skipped Library entry {:?} because its metadata is unavailable: {error}",
                        entry.file_name()
                    ));
                    continue;
                }
            };
            if metadata.is_file() && !metadata_is_link_or_reparse(&metadata) {
                self.consider_file(
                    entry.path(),
                    Some((session.clone(), session_game.clone())),
                    metadata,
                    candidates,
                    warnings,
                );
            }
        }
    }

    fn consider_file(
        &self,
        display_path: PathBuf,
        session: Option<(String, Option<ClipGame>)>,
        metadata: std::fs::Metadata,
        candidates: &mut CandidateSet,
        warnings: &mut WarningSink,
    ) {
        if display_path
            .extension()
            .and_then(|extension| extension.to_str())
            != Some("mp4")
        {
            return;
        }
        let canonical_path = match display_path.canonicalize() {
            Ok(path) => path,
            Err(error) => {
                warnings.push(format!(
                    "Skipped Library clip {:?} because it could not be canonicalized: {error}",
                    display_path
                ));
                return;
            }
        };
        let contained = canonical_path.parent() == Some(self.canonical_root.as_path())
            || canonical_path.parent().and_then(Path::parent)
                == Some(self.canonical_root.as_path());
        if !contained {
            warnings.push(format!(
                "Skipped Library clip {:?} because it escaped the media root",
                display_path
            ));
            return;
        }
        let display_path = display_path.display().to_string();
        let Some(identity) = ClipPathIdentity::from_text(&display_path) else {
            warnings.push(format!(
                "Skipped Library clip because its path identity is invalid: {display_path:?}"
            ));
            return;
        };
        let name = canonical_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let modified_unix = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_secs());
        let (session, session_game) =
            session.map_or((None, None), |(name, game)| (Some(name), game));
        candidates.push(ClipCandidate {
            display_path,
            canonical_path,
            identity,
            name,
            session,
            session_game,
            size_bytes: metadata.len(),
            modified_unix,
        });
    }

    fn build_source(&self, candidate: ClipCandidate, warnings: &mut WarningSink) -> ClipScanSource {
        let metadata = read_clip_metadata(&candidate.canonical_path, warnings);
        let title = metadata.as_ref().and_then(ClipMetadata::normalized_title);
        let kind = metadata
            .as_ref()
            .and_then(ClipMetadata::normalized_kind)
            .unwrap_or_else(|| inferred_clip_kind_for_path(&candidate.canonical_path).to_string());
        ClipScanSource {
            display_path: candidate.display_path,
            canonical_path: candidate.canonical_path,
            name: candidate.name,
            title,
            kind,
            session: candidate.session,
            size_bytes: candidate.size_bytes,
            modified_unix: candidate.modified_unix,
            session_game: candidate.session_game,
        }
    }
}

#[derive(Debug, Clone)]
struct ClipCandidate {
    display_path: String,
    canonical_path: PathBuf,
    identity: ClipPathIdentity,
    name: String,
    session: Option<String>,
    session_game: Option<ClipGame>,
    size_bytes: u64,
    modified_unix: u64,
}

impl PartialEq for ClipCandidate {
    fn eq(&self, other: &Self) -> bool {
        self.modified_unix == other.modified_unix
            && self.identity == other.identity
            && self.display_path == other.display_path
    }
}

impl Eq for ClipCandidate {}

impl PartialOrd for ClipCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ClipCandidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.modified_unix
            .cmp(&other.modified_unix)
            .then_with(|| other.identity.cmp(&self.identity))
            .then_with(|| other.display_path.cmp(&self.display_path))
    }
}

#[derive(Default)]
struct CandidateSet {
    // Reverse ordering without wrapping the entire candidate keeps the source
    // available when the oldest/worst retained row is evicted.
    worst_first: BinaryHeap<std::cmp::Reverse<ClipCandidate>>,
    accepted: usize,
}

impl CandidateSet {
    fn push(&mut self, candidate: ClipCandidate) {
        self.accepted = self.accepted.saturating_add(1);
        if self.worst_first.len() < MAX_LOCAL_INDEX_ROWS {
            self.worst_first.push(std::cmp::Reverse(candidate));
            return;
        }
        let Some(mut worst) = self.worst_first.peek_mut() else {
            return;
        };
        if candidate > worst.0 {
            *worst = std::cmp::Reverse(candidate);
        }
    }

    fn truncated(&self) -> bool {
        self.accepted > MAX_LOCAL_INDEX_ROWS
    }

    fn into_ordered(self) -> Vec<ClipCandidate> {
        let mut candidates: Vec<_> = self
            .worst_first
            .into_iter()
            .map(|candidate| candidate.0)
            .collect();
        candidates.sort_unstable_by(|left, right| right.cmp(left));
        candidates
    }
}

#[derive(Debug, Default, Deserialize)]
struct ClipMetadata {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    kind: Option<String>,
}

impl ClipMetadata {
    fn normalized_title(&self) -> Option<String> {
        self.title
            .as_deref()
            .map(str::trim)
            .filter(|title| !title.is_empty() && title.len() <= MAX_CATALOG_STRING_BYTES)
            .map(ToOwned::to_owned)
    }

    fn normalized_kind(&self) -> Option<String> {
        self.kind
            .as_deref()
            .map(str::trim)
            .filter(|kind| matches!(*kind, "replay" | "session" | "trim"))
            .map(ToOwned::to_owned)
    }
}

fn read_clip_metadata(path: &Path, warnings: &mut WarningSink) -> Option<ClipMetadata> {
    let sidecar = path.with_extension("clipline.json");
    match read_bounded_json(&sidecar, MAX_CLIP_METADATA_BYTES) {
        Ok(value) => value,
        Err(error) => {
            warnings.push(format!(
                "Skipped invalid clip metadata {:?}: {error}",
                sidecar
            ));
            None
        }
    }
}

fn read_session_game(directory: &Path, warnings: &mut WarningSink) -> Option<ClipGame> {
    let sidecar = directory.join("clipline-session.json");
    let game = match read_bounded_json::<ClipGame>(&sidecar, MAX_SESSION_GAME_BYTES) {
        Ok(game) => game,
        Err(error) => {
            warnings.push(format!(
                "Skipped invalid session game {:?}: {error}",
                sidecar
            ));
            None
        }
    }?;
    if game.id.trim().is_empty()
        || game.name.trim().is_empty()
        || game.id.len() > MAX_CATALOG_STRING_BYTES
        || game.name.len() > MAX_CATALOG_STRING_BYTES
    {
        warnings.push(format!(
            "Skipped invalid session game {:?}: fields are empty or too large",
            sidecar
        ));
        return None;
    }
    Some(game)
}

fn read_bounded_json<T: for<'de> Deserialize<'de>>(
    path: &Path,
    maximum: usize,
) -> Result<Option<T>, String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    if metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
        return Err("sidecar is not a regular owned file".into());
    }
    let file = open_regular_file_nofollow(path).map_err(|error| error.to_string())?;
    let opened_length = file.metadata().map_err(|error| error.to_string())?.len();
    if opened_length > maximum as u64 {
        return Err(format!(
            "sidecar is {} bytes; maximum is {maximum}",
            opened_length
        ));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(opened_length).unwrap_or(maximum));
    file.take(u64::try_from(maximum).unwrap_or(u64::MAX) + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > maximum {
        return Err(format!(
            "sidecar is at least {} bytes; maximum is {maximum}",
            bytes.len()
        ));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| error.to_string())
}

#[derive(Default)]
struct WarningSink {
    warnings: Vec<String>,
    omitted: usize,
}

impl WarningSink {
    fn push(&mut self, warning: String) {
        if self.warnings.len() < MAX_LOCAL_SCAN_WARNINGS.saturating_sub(1) {
            self.warnings.push(warning);
        } else {
            self.omitted = self.omitted.saturating_add(1);
        }
    }

    fn push_required(&mut self, warning: String) {
        if self.warnings.len() >= MAX_LOCAL_SCAN_WARNINGS.saturating_sub(1)
            && self.warnings.pop().is_some()
        {
            self.omitted = self.omitted.saturating_add(1);
        }
        self.warnings.push(warning);
    }

    fn finish(mut self) -> Vec<String> {
        if self.omitted != 0 {
            self.warnings.push(format!(
                "Library scan omitted {} additional warnings.",
                self.omitted
            ));
        }
        self.warnings
    }
}

fn metadata_is_link_or_reparse(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}
