//! Bounded marker-sidecar parsing and active-clip detail projection.
//!
//! The repository scan has two consumers with deliberately different ownership:
//! the Tauri compatibility adapter needs the complete (but bounded) `ClipMarkers`
//! document, while the native catalog keeps only [`MarkerSidecarSummary`] for each
//! row. This module parses once and exposes both projections without retaining the
//! JSON input bytes.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use clipline_events::{is_review_event, ClipAudioTrack, ClipMarkers};
use clipline_shell::open_regular_file_nofollow;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{
    marker_digest, ClipDetail, ClipDetailAudioTrack, ClipDetailError, ClipDetailRequest,
    ClipDetailResult, GalleryMarker, MarkerTick, PlayerCardSummary, UploadDialogSummary,
    MAX_CATALOG_STRING_BYTES, MAX_CLIP_DETAIL_AUDIO_TRACKS, MAX_CLIP_DETAIL_FIELD_BYTES,
    MAX_CLIP_DETAIL_MARKERS, MAX_CLIP_DETAIL_SIDECAR_BYTES,
};

/// Maximum object/array depth accepted before JSON deserialization.
pub const MAX_CLIP_SIDECAR_JSON_DEPTH: usize = 32;
/// Maximum plays retained in a bounded compatibility marker document.
// The recorder retains up to 512 osu! title transitions. The API path retains
// at most 500 scores, so 512 is the compatibility ceiling for either source.
pub const MAX_CLIP_SIDECAR_PLAYS: usize = 512;
/// Maximum total entries in all marker-sidecar vectors, including nested vectors.
pub const MAX_CLIP_SIDECAR_NESTED_ENTRIES: usize = 50_000;
/// Maximum total JSON object members and array elements, including unknown
/// fields that serde will ignore after the envelope is validated.
pub const MAX_CLIP_SIDECAR_JSON_ENTRIES: usize = 250_000;
/// Maximum decoded UTF-8 bytes across string values in a marker sidecar.
///
/// This is intentionally larger than the foreground detail string budget: enum
/// names occur once per marker in the compatibility document, while the compact
/// projection remains capped by `MAX_CLIP_DETAIL_FIELD_BYTES`.
pub const MAX_CLIP_SIDECAR_STRING_BYTES: usize = 6 * 1024 * 1024;
/// Longest timeline accepted from one marker sidecar (seven continuous days).
pub const MAX_CLIP_TIMELINE_SECONDS: f64 = 7.0 * 24.0 * 60.0 * 60.0;

const _: () = assert!(MAX_CLIP_SIDECAR_JSON_DEPTH > 0);
const _: () = assert!(MAX_CLIP_SIDECAR_PLAYS <= MAX_CLIP_SIDECAR_NESTED_ENTRIES);
const _: () = assert!(MAX_CLIP_DETAIL_MARKERS <= MAX_CLIP_SIDECAR_NESTED_ENTRIES);
const _: () = assert!(MAX_CLIP_DETAIL_AUDIO_TRACKS <= MAX_CLIP_SIDECAR_NESTED_ENTRIES);

#[derive(Debug, Error)]
pub enum LocalSidecarError {
    #[error("read marker sidecar {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{field} contains {actual} bytes or entries; maximum is {maximum}")]
    TooLarge {
        field: &'static str,
        actual: usize,
        maximum: usize,
    },
    #[error("marker sidecar JSON is invalid: {source}")]
    InvalidJson {
        #[source]
        source: serde_json::Error,
    },
    #[error("{field} contains an invalid timeline value")]
    InvalidTimeline { field: &'static str },
    #[error("marker sidecar projection is invalid: {0}")]
    Projection(String),
    #[error("marker sidecar is not a regular contained file: {path}")]
    UnsafeFileType { path: PathBuf },
    #[error("could not reserve bounded marker-sidecar storage")]
    Allocation,
}

impl From<ClipDetailError> for LocalSidecarError {
    fn from(error: ClipDetailError) -> Self {
        Self::Projection(error.to_string())
    }
}

/// Aggregate result categories required by native gallery cards.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayOutcomeSummary {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub incomplete: usize,
}

/// Compact, bounded scan projection. It owns no per-marker, per-play, or
/// per-track vectors and is therefore safe to retain for all 10,000 rows.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MarkerSidecarSummary {
    pub duration_s: f64,
    pub review_marker_count: usize,
    pub marker_digest: String,
    pub audio_track_count: usize,
    pub plays: PlayOutcomeSummary,
    pub player_summary: Option<PlayerCardSummary>,
    pub search_text: String,
}

impl MarkerSidecarSummary {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// One validated parse with separate full and compact projections.
#[derive(Debug, Clone)]
pub struct ParsedMarkerSidecar {
    sidecar_bytes: usize,
    markers: ClipMarkers,
    summary: MarkerSidecarSummary,
}

impl ParsedMarkerSidecar {
    fn new(sidecar_bytes: usize, mut markers: ClipMarkers) -> Result<Self, LocalSidecarError> {
        validate_marker_document(&mut markers)?;
        // Apply the same review policy as the shipping Tauri library before
        // either projection can observe the document.
        markers
            .markers
            .retain(|marker| is_review_event(&marker.event));
        let summary = summarize_markers(&markers)?;
        Ok(Self {
            sidecar_bytes,
            markers,
            summary,
        })
    }

    #[must_use]
    pub const fn sidecar_bytes(&self) -> usize {
        self.sidecar_bytes
    }

    /// Complete bounded compatibility projection.
    #[must_use]
    pub const fn markers(&self) -> &ClipMarkers {
        &self.markers
    }

    #[must_use]
    pub fn into_markers(self) -> ClipMarkers {
        self.markers
    }

    /// Compact native scan projection.
    #[must_use]
    pub const fn summary(&self) -> &MarkerSidecarSummary {
        &self.summary
    }

    fn replace_audio_tracks(
        &mut self,
        audio_tracks: Vec<ClipAudioTrack>,
    ) -> Result<(), LocalSidecarError> {
        self.markers.audio_tracks = audio_tracks;
        self.summary = summarize_markers(&self.markers)?;
        Ok(())
    }

    fn detail(&self, title: &str, description: &str) -> Result<ClipDetail, LocalSidecarError> {
        let marker_ticks = self
            .markers
            .markers
            .iter()
            .map(|marker| MarkerTick::new(marker.t_s))
            .collect::<Result<Vec<_>, _>>()?;
        let audio_tracks = self
            .markers
            .audio_tracks
            .iter()
            .map(|track| ClipDetailAudioTrack::new(&track.id, &track.label))
            .collect::<Result<Vec<_>, _>>()?;
        let audio_summary = match audio_tracks.len() {
            0 => "No audio tracks".to_owned(),
            1 => "1 audio track".to_owned(),
            count => format!("{count} audio tracks"),
        };
        let upload = UploadDialogSummary::new(
            title,
            description,
            &self.summary.marker_digest,
            audio_summary,
        )?;
        ClipDetail::new(
            self.sidecar_bytes,
            marker_ticks,
            &self.summary.marker_digest,
            audio_tracks,
            upload,
        )
        .map_err(Into::into)
    }
}

/// Seam used by tests and repository adapters to infer tracks in pre-sidecar MP4s.
pub trait LegacyAudioTrackProbe {
    fn audio_track_count(&self, clip_path: &Path) -> Result<usize, String>;
}

/// Shipping MP4 metadata probe. It reads only the bounded MP4 metadata path in
/// `clipline-mp4`; no decoder or FFmpeg process is involved.
#[derive(Debug, Clone, Copy, Default)]
pub struct Mp4LegacyAudioTrackProbe;

impl LegacyAudioTrackProbe for Mp4LegacyAudioTrackProbe {
    fn audio_track_count(&self, clip_path: &Path) -> Result<usize, String> {
        clipline_mp4::media_track_counts_file(clip_path)
            .map(|counts| counts.audio)
            .map_err(|error| error.to_string())
    }
}

#[must_use]
pub fn marker_sidecar_path(clip_path: &Path) -> PathBuf {
    clip_path.with_extension("markers.json")
}

/// Parse a caller-owned byte slice into bounded full and compact projections.
/// The returned value never retains `bytes`.
pub fn parse_marker_sidecar(bytes: &[u8]) -> Result<ParsedMarkerSidecar, LocalSidecarError> {
    check_maximum("sidecar.bytes", bytes.len(), MAX_CLIP_DETAIL_SIDECAR_BYTES)?;
    validate_json_depth(bytes)?;
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|source| LocalSidecarError::InvalidJson { source })?;
    validate_json_strings(&value)?;
    let markers: ClipMarkers = serde_json::from_value(value)
        .map_err(|source| LocalSidecarError::InvalidJson { source })?;
    ParsedMarkerSidecar::new(bytes.len(), markers)
}

/// Parse and validate a bounded marker document without applying the catalog's
/// review-event filter. Mutation services use this form when replacing one
/// field while preserving every shipping compatibility marker byte-for-byte at
/// the data-model level.
pub fn parse_marker_sidecar_preserving_all(bytes: &[u8]) -> Result<ClipMarkers, LocalSidecarError> {
    check_maximum("sidecar.bytes", bytes.len(), MAX_CLIP_DETAIL_SIDECAR_BYTES)?;
    validate_json_depth(bytes)?;
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|source| LocalSidecarError::InvalidJson { source })?;
    validate_json_strings(&value)?;
    let mut markers: ClipMarkers = serde_json::from_value(value)
        .map_err(|source| LocalSidecarError::InvalidJson { source })?;
    validate_marker_document(&mut markers)?;
    Ok(markers)
}

/// Read `<clip>.markers.json` with an allocation and read ceiling. Missing
/// sidecars are represented as `None`; every other I/O or parse failure is typed.
pub fn read_marker_sidecar(
    clip_path: &Path,
) -> Result<Option<ParsedMarkerSidecar>, LocalSidecarError> {
    let path = marker_sidecar_path(clip_path);
    let path_metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(LocalSidecarError::Io {
                path: path.clone(),
                source,
            })
        }
    };
    if !path_metadata.is_file()
        || path_metadata.file_type().is_symlink()
        || has_windows_reparse_attribute(&path_metadata)
    {
        return Err(LocalSidecarError::UnsafeFileType { path });
    }
    let mut file = match open_regular_file_nofollow(&path) {
        Ok(file) => file,
        Err(source) => return Err(LocalSidecarError::Io { path, source }),
    };
    let declared = file
        .metadata()
        .map_err(|source| LocalSidecarError::Io {
            path: path.clone(),
            source,
        })?
        .len();
    if declared > MAX_CLIP_DETAIL_SIDECAR_BYTES as u64 {
        return Err(LocalSidecarError::TooLarge {
            field: "sidecar.bytes",
            actual: usize::try_from(declared).unwrap_or(usize::MAX),
            maximum: MAX_CLIP_DETAIL_SIDECAR_BYTES,
        });
    }
    let capacity = usize::try_from(declared).unwrap_or(MAX_CLIP_DETAIL_SIDECAR_BYTES);
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| LocalSidecarError::Allocation)?;
    file.by_ref()
        .take((MAX_CLIP_DETAIL_SIDECAR_BYTES as u64) + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| LocalSidecarError::Io {
            path: path.clone(),
            source,
        })?;
    check_maximum("sidecar.bytes", bytes.len(), MAX_CLIP_DETAIL_SIDECAR_BYTES)?;
    parse_marker_sidecar(&bytes).map(Some)
}

#[cfg(windows)]
fn has_windows_reparse_attribute(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
const fn has_windows_reparse_attribute(_metadata: &std::fs::Metadata) -> bool {
    false
}

/// Load the sidecar and infer stable legacy track descriptors only when no
/// declared descriptors exist. Probe failures preserve shipping behavior by
/// leaving the track list empty; hostile inferred counts still fail closed.
pub fn load_marker_sidecar_with_probe(
    clip_path: &Path,
    probe: &dyn LegacyAudioTrackProbe,
) -> Result<Option<ParsedMarkerSidecar>, LocalSidecarError> {
    let parsed = read_marker_sidecar(clip_path)?;
    if parsed
        .as_ref()
        .is_some_and(|parsed| !parsed.markers.audio_tracks.is_empty())
    {
        return Ok(parsed);
    }
    let Ok(audio_track_count) = probe.audio_track_count(clip_path) else {
        return Ok(parsed);
    };
    check_maximum(
        "sidecar.audio_tracks",
        audio_track_count,
        MAX_CLIP_DETAIL_AUDIO_TRACKS,
    )?;
    if audio_track_count == 0 {
        return Ok(parsed);
    }
    let audio_tracks = inferred_audio_tracks(audio_track_count);
    let mut parsed = match parsed {
        Some(parsed) => parsed,
        None => ParsedMarkerSidecar::new(0, empty_markers())?,
    };
    parsed.replace_audio_tracks(audio_tracks)?;
    Ok(Some(parsed))
}

/// Synchronously load one token-owned foreground detail result.
pub fn load_clip_detail(
    request: &ClipDetailRequest,
    clip_path: &Path,
    title: &str,
    description: &str,
    probe: &dyn LegacyAudioTrackProbe,
) -> Result<ClipDetailResult, LocalSidecarError> {
    let parsed = load_marker_sidecar_with_probe(clip_path, probe)?
        .unwrap_or(ParsedMarkerSidecar::new(0, empty_markers())?);
    let detail = parsed.detail(title, description)?;
    Ok(ClipDetailResult::new(request, detail))
}

fn empty_markers() -> ClipMarkers {
    ClipMarkers {
        recording_start_s: 0.0,
        duration_s: 0.0,
        player_summary: None,
        audio_tracks: Vec::new(),
        plays: Vec::new(),
        markers: Vec::new(),
    }
}

fn inferred_audio_tracks(count: usize) -> Vec<ClipAudioTrack> {
    (0..count)
        .map(|index| ClipAudioTrack {
            id: format!("audio:{index}"),
            track_index: u32::try_from(index).expect("bounded audio-track count fits u32"),
            label: format!("Audio Track {}", index + 1),
            kind: Some("inferred".into()),
        })
        .collect()
}

fn validate_marker_document(markers: &mut ClipMarkers) -> Result<(), LocalSidecarError> {
    check_maximum(
        "sidecar.markers",
        markers.markers.len(),
        MAX_CLIP_DETAIL_MARKERS,
    )?;
    check_maximum("sidecar.plays", markers.plays.len(), MAX_CLIP_SIDECAR_PLAYS)?;
    check_maximum(
        "sidecar.audio_tracks",
        markers.audio_tracks.len(),
        MAX_CLIP_DETAIL_AUDIO_TRACKS,
    )?;
    let mut audio_track_ids = BTreeSet::new();
    for track in &markers.audio_tracks {
        // Reuse the active-detail value contract at the parse boundary so a
        // row cannot scan successfully and then fail merely because it was
        // opened.
        ClipDetailAudioTrack::new(&track.id, &track.label)?;
        if !audio_track_ids.insert(track.id.as_str()) {
            return Err(LocalSidecarError::Projection(format!(
                "audio track id {:?} appears more than once",
                track.id
            )));
        }
    }

    let mut nested_entries = markers
        .markers
        .len()
        .checked_add(markers.plays.len())
        .and_then(|count| count.checked_add(markers.audio_tracks.len()))
        .ok_or(LocalSidecarError::TooLarge {
            field: "sidecar.nested_entries",
            actual: usize::MAX,
            maximum: MAX_CLIP_SIDECAR_NESTED_ENTRIES,
        })?;
    if let Some(player) = &markers.player_summary {
        add_entries(&mut nested_entries, player.participants.len())?;
        add_entries(&mut nested_entries, player.summoner_spells.len())?;
        add_entries(&mut nested_entries, player.items.len())?;
    }
    for marker in &markers.markers {
        add_entries(&mut nested_entries, marker.event.assisters.len())?;
    }
    for play in &markers.plays {
        add_entries(&mut nested_entries, play.mods.len())?;
    }
    check_maximum(
        "sidecar.nested_entries",
        nested_entries,
        MAX_CLIP_SIDECAR_NESTED_ENTRIES,
    )?;

    validate_nonnegative_finite("sidecar.recording_start_s", markers.recording_start_s)?;
    validate_duration("sidecar.duration_s", markers.duration_s)?;
    canonicalize_zero(&mut markers.recording_start_s);
    canonicalize_zero(&mut markers.duration_s);

    for marker in &mut markers.markers {
        validate_timeline_position("sidecar.marker.t_s", marker.t_s, markers.duration_s)?;
        validate_nonnegative_finite("sidecar.marker.game_time_s", marker.event.game_time_s)?;
        if let Some(offset) = marker.event.recording_offset_s {
            validate_nonnegative_finite("sidecar.marker.recording_offset_s", offset)?;
        }
        canonicalize_zero(&mut marker.t_s);
        canonicalize_zero(&mut marker.event.game_time_s);
        if let Some(offset) = &mut marker.event.recording_offset_s {
            canonicalize_zero(offset);
        }
    }
    for play in &mut markers.plays {
        validate_timeline_position("sidecar.play.t_start_s", play.t_start_s, markers.duration_s)?;
        if let Some(end) = play.t_end_s {
            validate_timeline_position("sidecar.play.t_end_s", end, markers.duration_s)?;
            if end < play.t_start_s {
                return Err(LocalSidecarError::InvalidTimeline {
                    field: "sidecar.play.range",
                });
            }
        }
        canonicalize_zero(&mut play.t_start_s);
        if let Some(end) = &mut play.t_end_s {
            canonicalize_zero(end);
        }
    }
    Ok(())
}

fn summarize_markers(markers: &ClipMarkers) -> Result<MarkerSidecarSummary, LocalSidecarError> {
    let gallery_markers = markers
        .markers
        .iter()
        .map(|marker| GalleryMarker::new(format!("{:?}", marker.event.kind)))
        .collect::<Vec<_>>();
    let marker_digest = bounded_catalog_text(
        &marker_digest(&gallery_markers, None)
            .map_err(|error| LocalSidecarError::Projection(error.to_string()))?,
    );
    let passed = markers.plays.iter().filter(|play| play.passed).count();
    let incomplete = markers
        .plays
        .iter()
        .filter(|play| {
            !play.passed
                && !play.pp.is_some_and(|pp| pp.is_finite() && pp > 0.0)
                && !play
                    .rank
                    .as_deref()
                    .is_some_and(|rank| rank.trim().eq_ignore_ascii_case("F"))
        })
        .count();
    let failed = markers.plays.len().saturating_sub(passed + incomplete);
    let player_summary = markers
        .player_summary
        .as_ref()
        .map(|summary| PlayerCardSummary {
            champion_name: bounded_catalog_text(&summary.champion_name),
            kills: summary.kills,
            deaths: summary.deaths,
            assists: summary.assists,
            creep_score: summary.creep_score,
            game_time_s: summary.game_time_s,
        });
    Ok(MarkerSidecarSummary {
        duration_s: markers.duration_s,
        review_marker_count: markers.markers.len(),
        marker_digest,
        audio_track_count: markers.audio_tracks.len(),
        plays: PlayOutcomeSummary {
            total: markers.plays.len(),
            passed,
            failed,
            incomplete,
        },
        player_summary,
        search_text: sidecar_search_text(markers),
    })
}

fn sidecar_search_text(markers: &ClipMarkers) -> String {
    let mut output = String::new();
    if let Some(player) = &markers.player_summary {
        append_search_term(&mut output, &player.champion_name);
        append_search_term(&mut output, &player.player_name);
        append_search_term(&mut output, &player.team);
        for participant in &player.participants {
            append_search_term(&mut output, &participant.player_name);
            append_search_term(&mut output, &participant.champion_name);
            append_search_term(&mut output, &participant.team);
        }
        for spell in &player.summoner_spells {
            append_search_term(&mut output, &spell.name);
        }
        for item in &player.items {
            append_search_term(&mut output, &item.name);
        }
    }
    for play in &markers.plays {
        append_search_term(&mut output, &play.title);
        append_search_term(&mut output, &play.artist);
        append_search_term(&mut output, &play.difficulty);
        if let Some(mapper) = &play.mapper {
            append_search_term(&mut output, mapper);
        }
    }
    output
}

fn append_search_term(output: &mut String, term: &str) {
    let term = term.trim();
    if term.is_empty() {
        return;
    }
    let separator = usize::from(!output.is_empty());
    let Some(next_len) = output
        .len()
        .checked_add(separator)
        .and_then(|length| length.checked_add(term.len()))
    else {
        return;
    };
    if separator != 0 {
        if output.len() == MAX_CATALOG_STRING_BYTES {
            return;
        }
        output.push(' ');
    }
    let remaining = MAX_CATALOG_STRING_BYTES.saturating_sub(output.len());
    if next_len <= MAX_CATALOG_STRING_BYTES {
        output.push_str(term);
    } else {
        let mut end = remaining.min(term.len());
        while end != 0 && !term.is_char_boundary(end) {
            end -= 1;
        }
        output.push_str(&term[..end]);
    }
}

fn bounded_catalog_text(value: &str) -> String {
    if value.len() <= MAX_CATALOG_STRING_BYTES {
        return value.to_owned();
    }
    let mut end = MAX_CATALOG_STRING_BYTES;
    while end != 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn validate_json_depth(bytes: &[u8]) -> Result<(), LocalSidecarError> {
    let mut depth = 0_usize;
    let mut in_string = false;
    let mut escaped = false;
    for &byte in bytes {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth = depth.saturating_add(1);
                if depth > MAX_CLIP_SIDECAR_JSON_DEPTH {
                    return Err(LocalSidecarError::TooLarge {
                        field: "sidecar.json_depth",
                        actual: depth,
                        maximum: MAX_CLIP_SIDECAR_JSON_DEPTH,
                    });
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(())
}

fn validate_json_strings(value: &Value) -> Result<(), LocalSidecarError> {
    fn add_string(string: &str, total: &mut usize) -> Result<(), LocalSidecarError> {
        check_maximum("sidecar.string", string.len(), MAX_CLIP_DETAIL_FIELD_BYTES)?;
        *total = total
            .checked_add(string.len())
            .ok_or(LocalSidecarError::TooLarge {
                field: "sidecar.string_bytes",
                actual: usize::MAX,
                maximum: MAX_CLIP_SIDECAR_STRING_BYTES,
            })?;
        check_maximum(
            "sidecar.string_bytes",
            *total,
            MAX_CLIP_SIDECAR_STRING_BYTES,
        )
    }

    fn add_json_entries(count: usize, entries: &mut usize) -> Result<(), LocalSidecarError> {
        *entries = entries
            .checked_add(count)
            .ok_or(LocalSidecarError::TooLarge {
                field: "sidecar.json_entries",
                actual: usize::MAX,
                maximum: MAX_CLIP_SIDECAR_JSON_ENTRIES,
            })?;
        check_maximum(
            "sidecar.json_entries",
            *entries,
            MAX_CLIP_SIDECAR_JSON_ENTRIES,
        )
    }

    fn visit(
        value: &Value,
        total: &mut usize,
        entries: &mut usize,
    ) -> Result<(), LocalSidecarError> {
        match value {
            Value::String(string) => add_string(string, total),
            Value::Array(values) => {
                add_json_entries(values.len(), entries)?;
                for value in values {
                    visit(value, total, entries)?;
                }
                Ok(())
            }
            Value::Object(values) => {
                add_json_entries(values.len(), entries)?;
                for (key, value) in values {
                    add_string(key, total)?;
                    visit(value, total, entries)?;
                }
                Ok(())
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => Ok(()),
        }
    }

    visit(value, &mut 0, &mut 0)
}

fn validate_duration(field: &'static str, value: f64) -> Result<(), LocalSidecarError> {
    if !value.is_finite() || !(0.0..=MAX_CLIP_TIMELINE_SECONDS).contains(&value) {
        Err(LocalSidecarError::InvalidTimeline { field })
    } else {
        Ok(())
    }
}

fn validate_nonnegative_finite(field: &'static str, value: f64) -> Result<(), LocalSidecarError> {
    if !value.is_finite() || value < 0.0 {
        Err(LocalSidecarError::InvalidTimeline { field })
    } else {
        Ok(())
    }
}

fn validate_timeline_position(
    field: &'static str,
    value: f64,
    duration: f64,
) -> Result<(), LocalSidecarError> {
    if !value.is_finite() || value < 0.0 || value > duration {
        Err(LocalSidecarError::InvalidTimeline { field })
    } else {
        Ok(())
    }
}

fn canonicalize_zero(value: &mut f64) {
    if *value == 0.0 {
        *value = 0.0;
    }
}

fn add_entries(total: &mut usize, count: usize) -> Result<(), LocalSidecarError> {
    *total = total
        .checked_add(count)
        .ok_or(LocalSidecarError::TooLarge {
            field: "sidecar.nested_entries",
            actual: usize::MAX,
            maximum: MAX_CLIP_SIDECAR_NESTED_ENTRIES,
        })?;
    check_maximum(
        "sidecar.nested_entries",
        *total,
        MAX_CLIP_SIDECAR_NESTED_ENTRIES,
    )
}

fn check_maximum(
    field: &'static str,
    actual: usize,
    maximum: usize,
) -> Result<(), LocalSidecarError> {
    if actual > maximum {
        Err(LocalSidecarError::TooLarge {
            field,
            actual,
            maximum,
        })
    } else {
        Ok(())
    }
}
