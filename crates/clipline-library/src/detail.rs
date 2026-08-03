use std::collections::BTreeSet;

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::{ClipPathIdentity, WindowWorkToken};

/// Maximum marker sidecar size accepted before parsing one active clip's detail.
pub const MAX_CLIP_DETAIL_SIDECAR_BYTES: usize = 8 * 1024 * 1024;
/// Maximum timeline markers retained in one active clip's detail.
pub const MAX_CLIP_DETAIL_MARKERS: usize = 10_000;
/// Maximum selectable audio tracks retained in one active clip's detail.
pub const MAX_CLIP_DETAIL_AUDIO_TRACKS: usize = 64;
/// Maximum aggregate UTF-8 bytes owned by all strings in one active clip's detail.
pub const MAX_CLIP_DETAIL_STRING_BYTES: usize = 256 * 1024;
/// Maximum UTF-8 bytes in any individual detail string.
pub const MAX_CLIP_DETAIL_FIELD_BYTES: usize = 64 * 1024;
/// Browser-compatible upload-title limit, measured as JavaScript UTF-16 code units.
pub const MAX_UPLOAD_TITLE_UTF16: usize = 140;
/// Browser-compatible upload-description limit, measured as JavaScript UTF-16 code units.
pub const MAX_UPLOAD_DESCRIPTION_UTF16: usize = 5_000;

const _: () = assert!(MAX_CLIP_DETAIL_AUDIO_TRACKS <= MAX_CLIP_DETAIL_MARKERS);
const _: () = assert!(MAX_CLIP_DETAIL_FIELD_BYTES <= MAX_CLIP_DETAIL_STRING_BYTES);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ClipDetailError {
    #[error("{field} contains {actual} bytes or entries; maximum is {maximum}")]
    TooLarge {
        field: &'static str,
        actual: usize,
        maximum: usize,
    },
    #[error("{field} is invalid")]
    Invalid { field: &'static str },
    #[error("audio track id {id:?} appears more than once")]
    DuplicateAudioTrackId { id: String },
}

fn check_maximum(
    field: &'static str,
    actual: usize,
    maximum: usize,
) -> Result<(), ClipDetailError> {
    if actual > maximum {
        Err(ClipDetailError::TooLarge {
            field,
            actual,
            maximum,
        })
    } else {
        Ok(())
    }
}

fn check_field_string(field: &'static str, value: &str) -> Result<(), ClipDetailError> {
    check_maximum(field, value.len(), MAX_CLIP_DETAIL_FIELD_BYTES)
}

fn checked_add_string_bytes(total: &mut usize, value: &str) -> Result<(), ClipDetailError> {
    *total = total
        .checked_add(value.len())
        .ok_or(ClipDetailError::TooLarge {
            field: "detail.string_bytes",
            actual: usize::MAX,
            maximum: MAX_CLIP_DETAIL_STRING_BYTES,
        })?;
    check_maximum("detail.string_bytes", *total, MAX_CLIP_DETAIL_STRING_BYTES)
}

/// Exact ownership fence for a single item's foreground detail request.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClipDetailOwner {
    item: ClipPathIdentity,
    window: WindowWorkToken,
}

impl ClipDetailOwner {
    #[must_use]
    pub const fn new(item: ClipPathIdentity, window: WindowWorkToken) -> Self {
        Self { item, window }
    }

    #[must_use]
    pub const fn item(&self) -> &ClipPathIdentity {
        &self.item
    }

    #[must_use]
    pub const fn window(&self) -> WindowWorkToken {
        self.window
    }
}

/// Request for bounded detail belonging to one stable catalog item.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClipDetailRequest {
    owner: ClipDetailOwner,
}

impl ClipDetailRequest {
    #[must_use]
    pub const fn new(item: ClipPathIdentity, window: WindowWorkToken) -> Self {
        Self {
            owner: ClipDetailOwner::new(item, window),
        }
    }

    #[must_use]
    pub const fn owner(&self) -> &ClipDetailOwner {
        &self.owner
    }

    #[must_use]
    pub const fn item(&self) -> &ClipPathIdentity {
        self.owner.item()
    }

    #[must_use]
    pub const fn window(&self) -> WindowWorkToken {
        self.owner.window()
    }
}

/// A marker position measured in seconds on the saved clip timeline.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct MarkerTick(f64);

impl MarkerTick {
    pub fn new(seconds: f64) -> Result<Self, ClipDetailError> {
        if !seconds.is_finite() || seconds < 0.0 {
            return Err(ClipDetailError::Invalid {
                field: "detail.marker_tick",
            });
        }
        // Canonicalize negative zero so equality and serialized evidence are stable.
        Ok(Self(if seconds == 0.0 { 0.0 } else { seconds }))
    }

    #[must_use]
    pub const fn seconds(self) -> f64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for MarkerTick {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let seconds = f64::deserialize(deserializer)?;
        Self::new(seconds).map_err(serde::de::Error::custom)
    }
}

/// Stable selectable audio identity and its already-resolved UI label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClipDetailAudioTrack {
    id: String,
    label: String,
}

impl ClipDetailAudioTrack {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Result<Self, ClipDetailError> {
        let id = id.into();
        let label = label.into();
        if id.trim().is_empty() {
            return Err(ClipDetailError::Invalid {
                field: "detail.audio_track.id",
            });
        }
        if label.trim().is_empty() {
            return Err(ClipDetailError::Invalid {
                field: "detail.audio_track.label",
            });
        }
        check_field_string("detail.audio_track.id", &id)?;
        check_field_string("detail.audio_track.label", &label)?;
        Ok(Self { id, label })
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }
}

impl<'de> Deserialize<'de> for ClipDetailAudioTrack {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawAudioTrack {
            id: String,
            label: String,
        }

        let raw = RawAudioTrack::deserialize(deserializer)?;
        Self::new(raw.id, raw.label).map_err(serde::de::Error::custom)
    }
}

/// Strings needed to initialize the upload dialog without retaining a sidecar document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UploadDialogSummary {
    title: String,
    description: String,
    marker_summary: String,
    audio_summary: String,
}

impl UploadDialogSummary {
    pub fn new(
        title: impl Into<String>,
        description: impl Into<String>,
        marker_summary: impl Into<String>,
        audio_summary: impl Into<String>,
    ) -> Result<Self, ClipDetailError> {
        let title = title.into();
        let description = description.into();
        let marker_summary = marker_summary.into();
        let audio_summary = audio_summary.into();

        check_maximum(
            "detail.upload.title_utf16",
            title.encode_utf16().count(),
            MAX_UPLOAD_TITLE_UTF16,
        )?;
        check_maximum(
            "detail.upload.description_utf16",
            description.encode_utf16().count(),
            MAX_UPLOAD_DESCRIPTION_UTF16,
        )?;
        check_field_string("detail.upload.title", &title)?;
        check_field_string("detail.upload.description", &description)?;
        check_field_string("detail.upload.marker_summary", &marker_summary)?;
        check_field_string("detail.upload.audio_summary", &audio_summary)?;

        Ok(Self {
            title,
            description,
            marker_summary,
            audio_summary,
        })
    }

    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    #[must_use]
    pub fn marker_summary(&self) -> &str {
        &self.marker_summary
    }

    #[must_use]
    pub fn audio_summary(&self) -> &str {
        &self.audio_summary
    }

    fn add_string_bytes(&self, total: &mut usize) -> Result<(), ClipDetailError> {
        checked_add_string_bytes(total, &self.title)?;
        checked_add_string_bytes(total, &self.description)?;
        checked_add_string_bytes(total, &self.marker_summary)?;
        checked_add_string_bytes(total, &self.audio_summary)
    }
}

impl<'de> Deserialize<'de> for UploadDialogSummary {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawUploadDialogSummary {
            title: String,
            description: String,
            marker_summary: String,
            audio_summary: String,
        }

        let raw = RawUploadDialogSummary::deserialize(deserializer)?;
        Self::new(
            raw.title,
            raw.description,
            raw.marker_summary,
            raw.audio_summary,
        )
        .map_err(serde::de::Error::custom)
    }
}

/// Bounded active-item detail. It never owns the raw sidecar bytes.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClipDetail {
    sidecar_bytes: usize,
    marker_ticks: Vec<MarkerTick>,
    marker_digest: String,
    audio_tracks: Vec<ClipDetailAudioTrack>,
    upload: UploadDialogSummary,
}

impl ClipDetail {
    pub fn new(
        sidecar_bytes: usize,
        marker_ticks: Vec<MarkerTick>,
        marker_digest: impl Into<String>,
        audio_tracks: Vec<ClipDetailAudioTrack>,
        upload: UploadDialogSummary,
    ) -> Result<Self, ClipDetailError> {
        check_maximum(
            "detail.sidecar_bytes",
            sidecar_bytes,
            MAX_CLIP_DETAIL_SIDECAR_BYTES,
        )?;
        check_maximum(
            "detail.marker_ticks",
            marker_ticks.len(),
            MAX_CLIP_DETAIL_MARKERS,
        )?;
        check_maximum(
            "detail.audio_tracks",
            audio_tracks.len(),
            MAX_CLIP_DETAIL_AUDIO_TRACKS,
        )?;

        let marker_digest = marker_digest.into();
        check_field_string("detail.marker_digest", &marker_digest)?;

        let mut ids = BTreeSet::new();
        for track in &audio_tracks {
            if !ids.insert(track.id()) {
                return Err(ClipDetailError::DuplicateAudioTrackId {
                    id: track.id().to_owned(),
                });
            }
        }

        let mut string_bytes = 0_usize;
        checked_add_string_bytes(&mut string_bytes, &marker_digest)?;
        for track in &audio_tracks {
            checked_add_string_bytes(&mut string_bytes, track.id())?;
            checked_add_string_bytes(&mut string_bytes, track.label())?;
        }
        upload.add_string_bytes(&mut string_bytes)?;

        Ok(Self {
            sidecar_bytes,
            marker_ticks,
            marker_digest,
            audio_tracks,
            upload,
        })
    }

    #[must_use]
    pub const fn sidecar_bytes(&self) -> usize {
        self.sidecar_bytes
    }

    #[must_use]
    pub fn marker_ticks(&self) -> &[MarkerTick] {
        &self.marker_ticks
    }

    #[must_use]
    pub fn marker_digest(&self) -> &str {
        &self.marker_digest
    }

    #[must_use]
    pub fn audio_tracks(&self) -> &[ClipDetailAudioTrack] {
        &self.audio_tracks
    }

    #[must_use]
    pub const fn upload(&self) -> &UploadDialogSummary {
        &self.upload
    }
}

impl<'de> Deserialize<'de> for ClipDetail {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawClipDetail {
            sidecar_bytes: usize,
            marker_ticks: Vec<MarkerTick>,
            marker_digest: String,
            audio_tracks: Vec<ClipDetailAudioTrack>,
            upload: UploadDialogSummary,
        }

        let raw = RawClipDetail::deserialize(deserializer)?;
        Self::new(
            raw.sidecar_bytes,
            raw.marker_ticks,
            raw.marker_digest,
            raw.audio_tracks,
            raw.upload,
        )
        .map_err(serde::de::Error::custom)
    }
}

/// Token-fenced detail completion for one stable item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClipDetailResult {
    owner: ClipDetailOwner,
    detail: ClipDetail,
}

impl ClipDetailResult {
    #[must_use]
    pub fn new(request: &ClipDetailRequest, detail: ClipDetail) -> Self {
        Self {
            owner: request.owner.clone(),
            detail,
        }
    }

    #[must_use]
    pub const fn owner(&self) -> &ClipDetailOwner {
        &self.owner
    }

    #[must_use]
    pub const fn detail(&self) -> &ClipDetail {
        &self.detail
    }

    #[must_use]
    pub fn matches_request(&self, request: &ClipDetailRequest) -> bool {
        self.owner == request.owner
    }

    #[must_use]
    pub(crate) fn estimated_owned_bytes(&self) -> usize {
        let detail = &self.detail;
        std::mem::size_of::<Self>()
            .saturating_add(self.owner.item().owned_capacity())
            .saturating_add(
                detail
                    .marker_ticks
                    .capacity()
                    .saturating_mul(std::mem::size_of::<MarkerTick>()),
            )
            .saturating_add(detail.marker_digest.capacity())
            .saturating_add(
                detail
                    .audio_tracks
                    .capacity()
                    .saturating_mul(std::mem::size_of::<ClipDetailAudioTrack>()),
            )
            .saturating_add(
                detail
                    .audio_tracks
                    .iter()
                    .map(|track| track.id.capacity().saturating_add(track.label.capacity()))
                    .fold(0_usize, usize::saturating_add),
            )
            .saturating_add(detail.upload.title.capacity())
            .saturating_add(detail.upload.description.capacity())
            .saturating_add(detail.upload.marker_summary.capacity())
            .saturating_add(detail.upload.audio_summary.capacity())
    }

    #[must_use]
    pub fn into_detail(self) -> ClipDetail {
        self.detail
    }
}
