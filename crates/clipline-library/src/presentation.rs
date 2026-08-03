use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    build_local_gallery, page_info, CatalogItemIdentity, CatalogRevision, CatalogSource,
    CatalogUploadVisibility, ClipPathIdentity, CloudCatalogOwner, CloudLibraryItem,
    CloudPageNumber, LocalClipItem, LocalDayResolver, LocalGalleryOptions, LocalPageIndex,
    PayloadBoundsError, PlayOutcomeSummary, PosterStatus, PresentationPoster, PresentationRow,
    RemoteClipId, UploadSummary, MAX_CATALOG_PAGE_ROWS, MAX_CATALOG_STRING_BYTES,
    MAX_CLIP_DETAIL_AUDIO_TRACKS, MAX_CLIP_DETAIL_FIELD_BYTES, MAX_CLIP_DETAIL_MARKERS,
    MAX_LOCAL_INDEX_ROWS, MAX_POSTER_RESULT_ENTRIES, MAX_UPLOAD_SUMMARIES,
};

pub const MAX_PRESENTATION_ENTRIES: usize = 128;
pub const MAX_GALLERY_CARD_PLAYS: usize = 256;
pub const MAX_GALLERY_CARD_STATS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PresentationError {
    #[error("{field} contains {actual} bytes or entries; maximum is {maximum}")]
    TooLarge {
        field: &'static str,
        actual: usize,
        maximum: usize,
    },
    #[error("{field} is invalid")]
    Invalid { field: &'static str },
    #[error("could not reserve bounded storage for {field}")]
    Allocation { field: &'static str },
}

/// Testable allocation gate used by the pure projection builder.
///
/// Implementations may reject a named reservation before any published
/// projection exists. The production implementation pairs this hook with
/// `Vec::try_reserve_exact` for every retained variable-length collection.
pub trait ProjectionReservation {
    fn before_reserve(
        &self,
        field: &'static str,
        additional: usize,
    ) -> Result<(), PresentationError>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemProjectionReservation;

impl ProjectionReservation for SystemProjectionReservation {
    fn before_reserve(
        &self,
        _field: &'static str,
        _additional: usize,
    ) -> Result<(), PresentationError> {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogLoadState {
    Empty,
    Loading,
    Ready,
    Disconnected,
    Error { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CatalogGroupProjection {
    pub label: Option<String>,
    pub row_start: usize,
    pub row_count: usize,
    pub total_count: usize,
    pub start_in_group: usize,
}

/// Page truth exposed to the UI. `total` and `page_count` stay absent for
/// Cloud because a full server page only authorizes a speculative next probe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CatalogPageProjection {
    pub page: u32,
    pub page_count: Option<u32>,
    pub total: Option<usize>,
    pub start: usize,
    pub end: usize,
    pub has_previous: bool,
    pub has_next: bool,
    pub range_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CatalogMenuProjection {
    pub target: CatalogItemIdentity,
    pub can_review: bool,
    pub can_rename: bool,
    pub can_delete: bool,
    pub can_upload: bool,
    pub can_reveal: bool,
    pub can_open_browser: bool,
    pub can_copy_link: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogDialogKind {
    RenameTitle,
    RenameFile,
    Delete,
    Upload,
    CancelUpload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CatalogDialogProjection {
    pub kind: CatalogDialogKind,
    pub target: CatalogItemIdentity,
    pub title: String,
    pub message: String,
    pub confirm_label: String,
    pub destructive: bool,
    pub text_value: Option<String>,
    pub description: Option<String>,
    pub visibility: Option<CatalogUploadVisibility>,
    pub audio_tracks: Vec<CatalogDialogAudioTrackProjection>,
    pub delete_local_after_upload: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CatalogDialogAudioTrackProjection {
    pub id: String,
    pub label: String,
    pub selected: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum CatalogProjectionSource<'a> {
    Local {
        items: &'a [LocalClipItem],
        options: &'a LocalGalleryOptions,
        page: LocalPageIndex,
    },
    Cloud {
        owner: &'a CloudCatalogOwner,
        page: CloudPageNumber,
        items: &'a [CloudLibraryItem],
        has_next: bool,
    },
    CloudDisconnected,
}

/// Plain-data input for rebuilding the complete visible catalog projection.
/// It intentionally owns no controller, service, filesystem, network, image,
/// or UI handle.
#[derive(Debug, Clone)]
pub struct CatalogProjectionInput<'a> {
    pub revision: CatalogRevision,
    pub source: CatalogProjectionSource<'a>,
    pub gallery: &'a GalleryPresentation,
    /// Sorted, unique, local-only identities. The controller can retain this
    /// as one fallibly allocated vector and the projection uses binary search.
    pub selected: &'a [CatalogItemIdentity],
    pub active: Option<&'a CatalogItemIdentity>,
    pub posters: &'a BTreeMap<ClipPathIdentity, PosterStatus>,
    pub menu: Option<&'a CatalogMenuProjection>,
    pub dialog: Option<&'a CatalogDialogProjection>,
    pub uploads: &'a [UploadSummary],
    pub load_state: CatalogLoadState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CatalogProjection {
    pub revision: CatalogRevision,
    pub source: CatalogSource,
    pub load_state: CatalogLoadState,
    pub rows: Vec<PresentationRow>,
    pub groups: Vec<CatalogGroupProjection>,
    pub page: CatalogPageProjection,
    pub selected_count: usize,
    pub menu: Option<CatalogMenuProjection>,
    pub dialog: Option<CatalogDialogProjection>,
    pub uploads: Vec<UploadSummary>,
}

pub fn build_catalog_projection<R: ProjectionReservation + ?Sized>(
    input: &CatalogProjectionInput<'_>,
    days: &dyn LocalDayResolver,
    reservation: &R,
) -> Result<CatalogProjection, PresentationError> {
    validate_projection_input(input)?;
    validate_card_presentation(input.gallery)?;

    let (source, rows, groups, page) = match input.source {
        CatalogProjectionSource::Local {
            items,
            options,
            page,
        } => build_local_projection(input, items, options, page, days, reservation)?,
        CatalogProjectionSource::Cloud {
            owner,
            page,
            items,
            has_next,
        } => build_cloud_projection(input, owner, page, items, has_next, reservation)?,
        CatalogProjectionSource::CloudDisconnected => disconnected_cloud_projection(),
    };

    let mut uploads = Vec::new();
    reserve_exact(
        reservation,
        &mut uploads,
        "projection.uploads",
        input.uploads.len(),
    )?;
    uploads.extend(input.uploads.iter().cloned());

    Ok(CatalogProjection {
        revision: input.revision,
        source,
        load_state: input.load_state.clone(),
        rows,
        groups,
        page,
        selected_count: input.selected.len(),
        menu: input.menu.cloned(),
        dialog: clone_dialog(input.dialog, reservation)?,
        uploads,
    })
}

fn validate_projection_input(input: &CatalogProjectionInput<'_>) -> Result<(), PresentationError> {
    check_count(
        "projection.selected",
        input.selected.len(),
        MAX_LOCAL_INDEX_ROWS,
    )?;
    check_count(
        "projection.posters",
        input.posters.len(),
        MAX_POSTER_RESULT_ENTRIES,
    )?;
    check_count(
        "projection.uploads",
        input.uploads.len(),
        MAX_UPLOAD_SUMMARIES,
    )?;
    if let CatalogLoadState::Error { message } = &input.load_state {
        check_string("projection.load_error", message)?;
    }
    for identity in input.selected {
        if identity.source() != CatalogSource::Local {
            return Err(PresentationError::Invalid {
                field: "projection.selected_identity",
            });
        }
    }
    if input.selected.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(PresentationError::Invalid {
            field: "projection.selected_order",
        });
    }
    for upload in input.uploads {
        upload.validate_bounds().map_err(bounds_to_presentation)?;
        if ClipPathIdentity::from_text(&upload.path).is_none() {
            return Err(PresentationError::Invalid {
                field: "projection.upload_path_identity",
            });
        }
    }
    for status in input.posters.values() {
        match status {
            PosterStatus::Ready { path } => check_string("projection.poster_path", path)?,
            PosterStatus::Failed { message } => {
                check_string("projection.poster_error", message)?;
            }
            PosterStatus::Queued | PosterStatus::Missing => {}
        }
    }
    if let Some(dialog) = input.dialog {
        check_string("projection.dialog.title", &dialog.title)?;
        check_string("projection.dialog.message", &dialog.message)?;
        check_string("projection.dialog.confirm_label", &dialog.confirm_label)?;
        check_optional_string("projection.dialog.text_value", dialog.text_value.as_deref())?;
        if let Some(description) = dialog.description.as_deref() {
            check_count(
                "projection.dialog.description",
                description.len(),
                MAX_CLIP_DETAIL_FIELD_BYTES,
            )?;
        }
        check_count(
            "projection.dialog.audio_tracks",
            dialog.audio_tracks.len(),
            MAX_CLIP_DETAIL_AUDIO_TRACKS,
        )?;
        for (index, track) in dialog.audio_tracks.iter().enumerate() {
            check_count(
                "projection.dialog.audio_track_id",
                track.id.len(),
                MAX_CLIP_DETAIL_FIELD_BYTES,
            )?;
            check_count(
                "projection.dialog.audio_track_label",
                track.label.len(),
                MAX_CLIP_DETAIL_FIELD_BYTES,
            )?;
            if track.id.trim().is_empty()
                || dialog.audio_tracks[..index]
                    .iter()
                    .any(|previous| previous.id == track.id)
            {
                return Err(PresentationError::Invalid {
                    field: "projection.dialog.duplicate_audio_track_id",
                });
            }
        }
    }

    let source = match input.source {
        CatalogProjectionSource::Local { .. } => CatalogSource::Local,
        CatalogProjectionSource::Cloud { .. } | CatalogProjectionSource::CloudDisconnected => {
            CatalogSource::Cloud
        }
    };
    if matches!(source, CatalogSource::Cloud) && !input.selected.is_empty() {
        return Err(PresentationError::Invalid {
            field: "projection.cloud_selection",
        });
    }
    if matches!(input.source, CatalogProjectionSource::CloudDisconnected)
        && (input.active.is_some() || input.menu.is_some() || input.dialog.is_some())
    {
        return Err(PresentationError::Invalid {
            field: "projection.disconnected_cloud_target",
        });
    }
    for identity in input
        .active
        .into_iter()
        .chain(input.menu.map(|menu| &menu.target))
        .chain(input.dialog.map(|dialog| &dialog.target))
    {
        if identity.source() != source {
            return Err(PresentationError::Invalid {
                field: "projection.identity_source",
            });
        }
        if let CatalogProjectionSource::Cloud { owner, .. } = input.source {
            if !identity.matches_cloud_catalog_owner(owner) {
                return Err(PresentationError::Invalid {
                    field: "projection.cloud_identity_owner",
                });
            }
        }
    }
    Ok(())
}

fn disconnected_cloud_projection() -> (
    CatalogSource,
    Vec<PresentationRow>,
    Vec<CatalogGroupProjection>,
    CatalogPageProjection,
) {
    (
        CatalogSource::Cloud,
        Vec::new(),
        Vec::new(),
        CatalogPageProjection {
            page: 1,
            page_count: None,
            total: None,
            start: 0,
            end: 0,
            has_previous: false,
            has_next: false,
            range_text: "0".to_owned(),
        },
    )
}

fn build_local_projection<R: ProjectionReservation + ?Sized>(
    input: &CatalogProjectionInput<'_>,
    items: &[LocalClipItem],
    options: &LocalGalleryOptions,
    requested_page: LocalPageIndex,
    days: &dyn LocalDayResolver,
    reservation: &R,
) -> Result<
    (
        CatalogSource,
        Vec<PresentationRow>,
        Vec<CatalogGroupProjection>,
        CatalogPageProjection,
    ),
    PresentationError,
> {
    check_count("projection.local_items", items.len(), MAX_LOCAL_INDEX_ROWS)?;
    check_string("projection.query", &options.query)?;
    for item in items {
        item.validate_bounds().map_err(bounds_to_presentation)?;
    }

    reservation.before_reserve("projection.local_index", items.len())?;
    let gallery = build_local_gallery(items, options, days);
    let info = page_info(
        requested_page.get() as usize,
        gallery.items.len(),
        MAX_CATALOG_PAGE_ROWS,
    );

    let mut rows = Vec::new();
    reserve_exact(
        reservation,
        &mut rows,
        "projection.rows",
        info.end.saturating_sub(info.start),
    )?;
    let mut groups = Vec::new();
    reserve_exact(
        reservation,
        &mut groups,
        "projection.groups",
        gallery.groups.len().min(MAX_CATALOG_PAGE_ROWS),
    )?;

    let mut group_offset = 0_usize;
    for group in &gallery.groups {
        let group_start = group_offset;
        let group_end = group_start.saturating_add(group.items.len());
        group_offset = group_end;
        let visible_start = info.start.max(group_start);
        let visible_end = info.end.min(group_end);
        if visible_start >= visible_end {
            continue;
        }
        let start_in_group = visible_start - group_start;
        let end_in_group = visible_end - group_start;
        let row_start = rows.len();
        if let Some(label) = &group.label {
            check_string("projection.group_label", label)?;
        }
        for item in &group.items[start_in_group..end_in_group] {
            rows.push(project_local_row(input, item, days)?);
        }
        groups.push(CatalogGroupProjection {
            label: group.label.clone(),
            row_start,
            row_count: end_in_group - start_in_group,
            total_count: group.items.len(),
            start_in_group,
        });
    }
    if rows.len() > MAX_CATALOG_PAGE_ROWS || groups.len() > MAX_CATALOG_PAGE_ROWS {
        return Err(PresentationError::Invalid {
            field: "projection.local_window",
        });
    }

    let (start, end, range_text) = if info.total == 0 {
        (0, 0, "0 of 0".to_owned())
    } else {
        let start = info.start + 1;
        let end = info.end;
        (start, end, format!("{start}–{end} of {}", info.total))
    };
    Ok((
        CatalogSource::Local,
        rows,
        groups,
        CatalogPageProjection {
            page: u32::try_from(info.page.saturating_add(1)).map_err(|_| {
                PresentationError::Invalid {
                    field: "projection.local_page",
                }
            })?,
            page_count: Some(u32::try_from(info.page_count).map_err(|_| {
                PresentationError::Invalid {
                    field: "projection.local_page_count",
                }
            })?),
            total: Some(info.total),
            start,
            end,
            has_previous: info.has_previous,
            has_next: info.has_next,
            range_text,
        },
    ))
}

fn build_cloud_projection<R: ProjectionReservation + ?Sized>(
    input: &CatalogProjectionInput<'_>,
    owner: &CloudCatalogOwner,
    page: CloudPageNumber,
    items: &[CloudLibraryItem],
    has_next: bool,
    reservation: &R,
) -> Result<
    (
        CatalogSource,
        Vec<PresentationRow>,
        Vec<CatalogGroupProjection>,
        CatalogPageProjection,
    ),
    PresentationError,
> {
    check_count("projection.cloud_items", items.len(), MAX_CATALOG_PAGE_ROWS)?;
    if items.is_empty() && page.get() > 1 {
        return Err(PresentationError::Invalid {
            field: "projection.cloud_empty_nonfirst",
        });
    }
    let mut rows = Vec::new();
    reserve_exact(reservation, &mut rows, "projection.rows", items.len())?;
    for item in items {
        item.validate_bounds().map_err(bounds_to_presentation)?;
        rows.push(project_cloud_row(input, owner, item)?);
    }
    let mut groups = Vec::new();
    reserve_exact(
        reservation,
        &mut groups,
        "projection.groups",
        usize::from(!rows.is_empty()),
    )?;
    if !rows.is_empty() {
        groups.push(CatalogGroupProjection {
            label: None,
            row_start: 0,
            row_count: rows.len(),
            total_count: rows.len(),
            start_in_group: 0,
        });
    }

    let page_offset = (page.get() as usize - 1).saturating_mul(MAX_CATALOG_PAGE_ROWS);
    let (start, end, range_text) = if rows.is_empty() {
        (0, 0, "0".to_owned())
    } else {
        let start = page_offset + 1;
        let end = page_offset.saturating_add(rows.len());
        (start, end, format!("{start}–{end}"))
    };
    Ok((
        CatalogSource::Cloud,
        rows,
        groups,
        CatalogPageProjection {
            page: page.get(),
            page_count: None,
            total: None,
            start,
            end,
            has_previous: page.get() > 1,
            has_next,
            range_text,
        },
    ))
}

fn project_local_row(
    input: &CatalogProjectionInput<'_>,
    item: &LocalClipItem,
    days: &dyn LocalDayResolver,
) -> Result<PresentationRow, PresentationError> {
    let path = item.path_identity().ok_or(PresentationError::Invalid {
        field: "local.path_identity",
    })?;
    let identity = CatalogItemIdentity::Local { path: path.clone() };
    let fallback = days.resolve_day(item.modified_unix).label;
    check_string("projection.local_fallback", &fallback)?;
    let preview = compact_gallery_card_preview(item, &fallback, input.gallery)?;
    let outcome_badge = play_result_summary_from_counts(item.marker_summary.plays);
    let marker_badge = if item.marker_summary.marker_digest.trim().is_empty() {
        match item.marker_count {
            0 => None,
            1 => Some("1 marker".to_owned()),
            count => Some(format!("{count} markers")),
        }
    } else {
        Some(item.marker_summary.marker_digest.trim().to_owned())
    };
    let matched_upload = input
        .uploads
        .iter()
        .find(|upload| ClipPathIdentity::from_text(&upload.path).as_ref() == Some(&path));
    let poster = input
        .posters
        .get(&path)
        .map(presentation_poster)
        .unwrap_or(PresentationPoster::Missing);
    let subtitle = if preview.summary.is_empty() {
        item.session
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| item.game.as_ref().map(|game| game.name.as_str()))
            .unwrap_or(&fallback)
            .trim()
            .to_owned()
    } else {
        preview.summary
    };
    let row = PresentationRow {
        identity: identity.clone(),
        path: item.path.clone(),
        title: preview.title,
        subtitle,
        duration: format_duration(item.duration_s.or_else(|| {
            (item.marker_summary.duration_s > 0.0).then_some(item.marker_summary.duration_s)
        })),
        kind: item.kind.clone(),
        selected: input.selected.binary_search(&identity).is_ok(),
        active: input.active == Some(&identity),
        game_badge: item
            .game
            .as_ref()
            .map(|game| game.name.trim().to_owned())
            .filter(|name| !name.is_empty()),
        marker_badge,
        outcome_badge: (!outcome_badge.is_empty()).then_some(outcome_badge),
        upload_badge: matched_upload.map(|upload| upload.upload_status.clone()),
        poster,
        warning: matched_upload.and_then(|upload| upload.error.clone()),
    };
    validate_presentation_row(&row)?;
    Ok(row)
}

fn project_cloud_row(
    input: &CatalogProjectionInput<'_>,
    owner: &CloudCatalogOwner,
    item: &CloudLibraryItem,
) -> Result<PresentationRow, PresentationError> {
    let remote_clip_id =
        RemoteClipId::new(item.remote_clip_id.clone()).map_err(|_| PresentationError::Invalid {
            field: "cloud.remote_clip_id",
        })?;
    let identity = CatalogItemIdentity::Cloud {
        account_key: owner.account_key.clone(),
        account_generation: owner.account_generation,
        remote_clip_id,
    };
    let row = PresentationRow {
        identity: identity.clone(),
        path: item.path.clone(),
        title: item.title.trim().to_owned(),
        subtitle: item.visibility.trim().to_owned(),
        duration: format_duration(item.duration_ms.map(|value| value as f64 / 1_000.0)),
        kind: item
            .source_type
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("cloud")
            .to_owned(),
        selected: false,
        active: input.active == Some(&identity),
        game_badge: None,
        marker_badge: None,
        outcome_badge: None,
        upload_badge: Some(item.upload_status.trim().to_owned()),
        poster: PresentationPoster::Missing,
        warning: None,
    };
    validate_presentation_row(&row)?;
    Ok(row)
}

fn compact_gallery_card_preview(
    item: &LocalClipItem,
    fallback_title: &str,
    presentation: &GalleryPresentation,
) -> Result<GalleryCardPreview, PresentationError> {
    validate_card_presentation(presentation)?;
    check_string("gallery.name", &item.name)?;
    check_optional_string("gallery.title", item.title.as_deref())?;
    check_string("gallery.kind", &item.kind)?;
    check_string("gallery.fallback_title", fallback_title)?;
    if let Some(summary) = &item.marker_summary.player_summary {
        check_string("gallery.summary.champion_name", &summary.champion_name)?;
    }
    gallery_card_preview_from_outcomes(
        &item.name,
        item.title.as_deref(),
        &item.kind,
        fallback_title,
        item.marker_summary.player_summary.as_ref(),
        item.marker_summary.plays,
        presentation,
    )
}

fn presentation_poster(status: &PosterStatus) -> PresentationPoster {
    match status {
        PosterStatus::Queued => PresentationPoster::Queued,
        PosterStatus::Ready { path } => PresentationPoster::Ready { path: path.clone() },
        PosterStatus::Missing => PresentationPoster::Missing,
        PosterStatus::Failed { message } => PresentationPoster::Failed {
            message: message.clone(),
        },
    }
}

fn validate_presentation_row(row: &PresentationRow) -> Result<(), PresentationError> {
    check_string("projection.row.path", &row.path)?;
    check_string("projection.row.title", &row.title)?;
    check_string("projection.row.subtitle", &row.subtitle)?;
    check_string("projection.row.duration", &row.duration)?;
    check_string("projection.row.kind", &row.kind)?;
    check_optional_string("projection.row.game_badge", row.game_badge.as_deref())?;
    check_optional_string("projection.row.marker_badge", row.marker_badge.as_deref())?;
    check_optional_string("projection.row.outcome_badge", row.outcome_badge.as_deref())?;
    check_optional_string("projection.row.upload_badge", row.upload_badge.as_deref())?;
    check_optional_string("projection.row.warning", row.warning.as_deref())?;
    match &row.poster {
        PresentationPoster::Ready { path } => check_string("projection.row.poster_path", path),
        PresentationPoster::Failed { message } => {
            check_string("projection.row.poster_error", message)
        }
        PresentationPoster::Queued | PresentationPoster::Missing => Ok(()),
    }
}

fn format_duration(duration: Option<f64>) -> String {
    let Some(duration) = duration.filter(|value| value.is_finite() && *value >= 0.0) else {
        return String::new();
    };
    let seconds = duration.round() as u64;
    let hours = seconds / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let seconds = seconds % 60;
    if hours == 0 {
        format!("{minutes}:{seconds:02}")
    } else {
        format!("{hours}:{minutes:02}:{seconds:02}")
    }
}

fn clone_dialog<R: ProjectionReservation + ?Sized>(
    dialog: Option<&CatalogDialogProjection>,
    reservation: &R,
) -> Result<Option<CatalogDialogProjection>, PresentationError> {
    let Some(dialog) = dialog else {
        return Ok(None);
    };
    let mut audio_tracks = Vec::new();
    reserve_exact(
        reservation,
        &mut audio_tracks,
        "projection.dialog_audio_tracks",
        dialog.audio_tracks.len(),
    )?;
    audio_tracks.extend(dialog.audio_tracks.iter().cloned());
    Ok(Some(CatalogDialogProjection {
        kind: dialog.kind,
        target: dialog.target.clone(),
        title: dialog.title.clone(),
        message: dialog.message.clone(),
        confirm_label: dialog.confirm_label.clone(),
        destructive: dialog.destructive,
        text_value: dialog.text_value.clone(),
        description: dialog.description.clone(),
        visibility: dialog.visibility,
        audio_tracks,
        delete_local_after_upload: dialog.delete_local_after_upload,
    }))
}

fn reserve_exact<T, R: ProjectionReservation + ?Sized>(
    reservation: &R,
    values: &mut Vec<T>,
    field: &'static str,
    additional: usize,
) -> Result<(), PresentationError> {
    if additional == 0 {
        return Ok(());
    }
    reservation.before_reserve(field, additional)?;
    values
        .try_reserve_exact(additional)
        .map_err(|_| PresentationError::Allocation { field })
}

fn bounds_to_presentation(error: PayloadBoundsError) -> PresentationError {
    match error {
        PayloadBoundsError::TooLarge {
            field,
            actual,
            maximum,
        } => PresentationError::TooLarge {
            field,
            actual,
            maximum,
        },
        PayloadBoundsError::Invalid { field } => PresentationError::Invalid { field },
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GalleryMarker {
    pub kind: String,
}

impl GalleryMarker {
    #[must_use]
    pub fn new(kind: impl Into<String>) -> Self {
        Self { kind: kind.into() }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkerKindPresentation {
    pub key: String,
    pub category: String,
    pub glyph: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkerCategoryPresentation {
    pub key: String,
    pub singular: String,
    pub plural: String,
    pub glyph: String,
    pub color: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkerPresentation {
    pub kinds: Vec<MarkerKindPresentation>,
    pub categories: Vec<MarkerCategoryPresentation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkerStyle {
    pub glyph: String,
    pub category: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

pub fn marker_style(
    kind: &str,
    presentation: Option<&MarkerPresentation>,
) -> Result<MarkerStyle, PresentationError> {
    validate_marker_inputs(&[GalleryMarker::new(kind)], presentation)?;
    let category = marker_category(kind, presentation);
    let meta = marker_category_meta(&category, presentation);
    let glyph = find_kind(kind, presentation)
        .map(|configured| configured.glyph.trim())
        .filter(|glyph| !glyph.is_empty())
        .unwrap_or(meta.glyph)
        .to_owned();
    Ok(MarkerStyle {
        glyph,
        category,
        color: meta.color.map(str::to_owned),
    })
}

pub fn marker_digest(
    markers: &[GalleryMarker],
    presentation: Option<&MarkerPresentation>,
) -> Result<String, PresentationError> {
    validate_marker_inputs(markers, presentation)?;
    let mut counts = BTreeMap::<String, usize>::new();
    for marker in markers {
        let category = marker_category(&marker.kind, presentation);
        *counts.entry(category).or_default() += 1;
    }

    let mut order = Vec::new();
    if let Some(presentation) = presentation {
        order.extend(
            presentation
                .categories
                .iter()
                .map(|category| category.key.as_str()),
        );
    }
    for category in DEFAULT_CATEGORIES {
        if !order.contains(&category.key) {
            order.push(category.key);
        }
    }
    let parts: Vec<_> = order
        .into_iter()
        .filter_map(|category| {
            let count = counts.get(category).copied()?;
            let meta = marker_category_meta(category, presentation);
            let label = if count == 1 {
                meta.singular
            } else {
                meta.plural
            };
            Some(format!("{count} {label}"))
        })
        .collect();
    Ok(parts.join(" · "))
}

#[derive(Debug, Clone, Copy)]
struct DefaultMarkerCategory {
    key: &'static str,
    singular: &'static str,
    plural: &'static str,
    glyph: &'static str,
    color: &'static str,
}

const DEFAULT_CATEGORIES: &[DefaultMarkerCategory] = &[
    DefaultMarkerCategory {
        key: "kill",
        singular: "kill",
        plural: "kills",
        glyph: "✕",
        color: "#ff9b9e",
    },
    DefaultMarkerCategory {
        key: "assist",
        singular: "assist",
        plural: "assists",
        glyph: "+",
        color: "#7bdff2",
    },
    DefaultMarkerCategory {
        key: "death",
        singular: "death",
        plural: "deaths",
        glyph: "✕",
        color: "#b9a7c4",
    },
    DefaultMarkerCategory {
        key: "spree",
        singular: "spree",
        plural: "sprees",
        glyph: "★",
        color: "#ffc77d",
    },
    DefaultMarkerCategory {
        key: "objective",
        singular: "objective",
        plural: "objectives",
        glyph: "◆",
        color: "#cdb2ff",
    },
    DefaultMarkerCategory {
        key: "structure",
        singular: "structure",
        plural: "structures",
        glyph: "▣",
        color: "#f0cd7a",
    },
    DefaultMarkerCategory {
        key: "info",
        singular: "event",
        plural: "events",
        glyph: "•",
        color: "#c0b4aa",
    },
];

struct MarkerCategoryMeta<'a> {
    singular: &'a str,
    plural: &'a str,
    glyph: &'a str,
    color: Option<&'a str>,
}

fn marker_category(kind: &str, presentation: Option<&MarkerPresentation>) -> String {
    find_kind(kind, presentation)
        .map(|configured| configured.category.trim())
        .filter(|category| !category.is_empty())
        .unwrap_or_else(|| default_marker_category(kind))
        .to_owned()
}

fn default_marker_category(kind: &str) -> &'static str {
    match kind {
        "ChampionKill" => "kill",
        "ChampionAssist" => "assist",
        "ChampionDeath" => "death",
        "FirstBlood" | "Multikill" | "Ace" => "spree",
        "DragonKill" | "HeraldKill" | "BaronKill" => "objective",
        "TurretKilled" | "InhibKilled" | "FirstBrick" => "structure",
        _ => "info",
    }
}

fn find_kind<'a>(
    kind: &str,
    presentation: Option<&'a MarkerPresentation>,
) -> Option<&'a MarkerKindPresentation> {
    presentation?.kinds.iter().find(|entry| entry.key == kind)
}

fn marker_category_meta<'a>(
    category: &str,
    presentation: Option<&'a MarkerPresentation>,
) -> MarkerCategoryMeta<'a> {
    let fallback = DEFAULT_CATEGORIES
        .iter()
        .find(|entry| entry.key == category)
        .unwrap_or_else(|| DEFAULT_CATEGORIES.last().expect("info category"));
    let configured = presentation.and_then(|presentation| {
        presentation
            .categories
            .iter()
            .find(|entry| entry.key == category)
    });
    MarkerCategoryMeta {
        singular: configured
            .map(|entry| entry.singular.trim())
            .filter(|value| !value.is_empty())
            .unwrap_or(fallback.singular),
        plural: configured
            .map(|entry| entry.plural.trim())
            .filter(|value| !value.is_empty())
            .unwrap_or(fallback.plural),
        glyph: configured
            .map(|entry| entry.glyph.trim())
            .filter(|value| !value.is_empty())
            .unwrap_or(fallback.glyph),
        color: configured
            .and_then(|entry| entry.color.as_deref())
            .map(str::trim)
            .filter(|color| !color.is_empty())
            .or(Some(fallback.color)),
    }
}

fn validate_marker_inputs(
    markers: &[GalleryMarker],
    presentation: Option<&MarkerPresentation>,
) -> Result<(), PresentationError> {
    check_count("markers", markers.len(), MAX_CLIP_DETAIL_MARKERS)?;
    for marker in markers {
        check_string("marker.kind", &marker.kind)?;
    }
    let Some(presentation) = presentation else {
        return Ok(());
    };
    check_count(
        "presentation.kinds",
        presentation.kinds.len(),
        MAX_PRESENTATION_ENTRIES,
    )?;
    check_count(
        "presentation.categories",
        presentation.categories.len(),
        MAX_PRESENTATION_ENTRIES,
    )?;
    let mut kind_keys = BTreeSet::new();
    for entry in &presentation.kinds {
        validate_key("presentation.kind.key", &entry.key)?;
        validate_key("presentation.kind.category", &entry.category)?;
        check_string("presentation.kind.glyph", &entry.glyph)?;
        if !kind_keys.insert(entry.key.as_str()) {
            return Err(PresentationError::Invalid {
                field: "presentation.kind.duplicate",
            });
        }
    }
    let mut category_keys = BTreeSet::new();
    for entry in &presentation.categories {
        validate_key("presentation.category.key", &entry.key)?;
        check_string("presentation.category.singular", &entry.singular)?;
        check_string("presentation.category.plural", &entry.plural)?;
        check_string("presentation.category.glyph", &entry.glyph)?;
        check_optional_string("presentation.category.color", entry.color.as_deref())?;
        if !category_keys.insert(entry.key.as_str()) {
            return Err(PresentationError::Invalid {
                field: "presentation.category.duplicate",
            });
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerCardSummary {
    pub champion_name: String,
    pub kills: u32,
    pub deaths: u32,
    pub assists: u32,
    pub creep_score: Option<u32>,
    pub game_time_s: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GalleryPlay {
    pub passed: bool,
    pub rank: String,
    pub pp: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GalleryCardInput {
    pub name: String,
    pub title: Option<String>,
    pub kind: String,
    pub fallback_title: String,
    pub player_summary: Option<PlayerCardSummary>,
    pub plays: Vec<GalleryPlay>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GallerySummaryMode {
    #[default]
    None,
    PlayerSummaryKda,
    OsuSetPlays,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GalleryCardTitlePolicy {
    #[default]
    Clip,
    Summary,
    SummaryForFullSession,
    OsuSessionSummary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GalleryCardStat {
    Kda,
    CsPerMin { label: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GalleryCardTitleFormat {
    pub separator: String,
    pub stats: Vec<GalleryCardStat>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetAlias {
    pub alias: String,
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GalleryCardIconConfig {
    Asset {
        url: String,
        label: String,
    },
    Portrait {
        label: String,
        aliases: Vec<AssetAlias>,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GalleryCardConfig {
    pub title: GalleryCardTitlePolicy,
    pub title_format: Option<GalleryCardTitleFormat>,
    pub icon: Option<GalleryCardIconConfig>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GalleryPresentation {
    pub summary: GallerySummaryMode,
    pub legacy_full_session_summary: bool,
    pub card: GalleryCardConfig,
    pub data_dragon_version: Option<String>,
    pub markers: MarkerPresentation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GalleryCardIcon {
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GalleryCardPreview {
    pub title: String,
    #[serde(rename = "titleSource")]
    pub title_source: String,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<GalleryCardIcon>,
}

pub fn gallery_card_preview(
    input: &GalleryCardInput,
    presentation: &GalleryPresentation,
) -> Result<GalleryCardPreview, PresentationError> {
    validate_card_input(input, presentation)?;
    gallery_card_preview_from_outcomes(
        &input.name,
        input.title.as_deref(),
        &input.kind,
        &input.fallback_title,
        input.player_summary.as_ref(),
        summarize_plays(&input.plays),
        presentation,
    )
}

fn gallery_card_preview_from_outcomes(
    name: &str,
    title: Option<&str>,
    kind: &str,
    fallback_title: &str,
    player_summary: Option<&PlayerCardSummary>,
    plays: PlayOutcomeSummary,
    presentation: &GalleryPresentation,
) -> Result<GalleryCardPreview, PresentationError> {
    let summary_label = match presentation.summary {
        GallerySummaryMode::PlayerSummaryKda => {
            player_summary.map(player_summary_label).unwrap_or_default()
        }
        GallerySummaryMode::OsuSetPlays => play_summary_from_count(plays.total),
        GallerySummaryMode::None => String::new(),
    };
    let detail_summary = match presentation.summary {
        GallerySummaryMode::OsuSetPlays => play_result_summary_from_counts(plays),
        _ => summary_label.clone(),
    };
    let formatted_summary = player_summary
        .zip(presentation.card.title_format.as_ref())
        .map(|(summary, format)| player_summary_stats_label(summary, format))
        .filter(|label| !label.is_empty())
        .unwrap_or_else(|| summary_label.clone());
    let fallback = fallback_title.trim();
    let custom_title = title.unwrap_or_default().trim();
    let clip_name = name.trim();
    let clip_display_title = if custom_title.is_empty() {
        strip_media_extension(clip_name)
    } else {
        custom_title.to_owned()
    };
    let policy = if presentation.legacy_full_session_summary
        && presentation.card.title == GalleryCardTitlePolicy::Clip
    {
        GalleryCardTitlePolicy::SummaryForFullSession
    } else {
        presentation.card.title
    };
    let uses_clip_title = policy == GalleryCardTitlePolicy::Clip
        || (policy == GalleryCardTitlePolicy::OsuSessionSummary && kind != "session");
    let uses_summary_title = !formatted_summary.is_empty()
        && matches!(
            (policy, kind),
            (GalleryCardTitlePolicy::Summary, _)
                | (GalleryCardTitlePolicy::SummaryForFullSession, "session")
                | (GalleryCardTitlePolicy::OsuSessionSummary, "session")
        );
    let clip_title = if uses_clip_title && !clip_display_title.is_empty() {
        clip_display_title
    } else {
        fallback.to_owned()
    };
    Ok(GalleryCardPreview {
        title: if uses_summary_title {
            formatted_summary
        } else {
            clip_title
        },
        title_source: if uses_summary_title {
            "summary"
        } else {
            "clip"
        }
        .into(),
        summary: detail_summary,
        icon: gallery_card_icon(player_summary, presentation)?,
    })
}

fn gallery_card_icon(
    summary: Option<&PlayerCardSummary>,
    presentation: &GalleryPresentation,
) -> Result<Option<GalleryCardIcon>, PresentationError> {
    let Some(config) = presentation.card.icon.as_ref() else {
        return Ok(None);
    };
    match config {
        GalleryCardIconConfig::Asset { url, label } => {
            let url = url.trim();
            if url.is_empty() {
                return Ok(None);
            }
            Ok(Some(GalleryCardIcon {
                kind: "asset".into(),
                url: url.into(),
                label: label.trim().into(),
            }))
        }
        GalleryCardIconConfig::Portrait { label, aliases } => {
            let Some(summary) = summary else {
                return Ok(None);
            };
            let champion = summary.champion_name.trim();
            let Some(version) = presentation
                .data_dragon_version
                .as_deref()
                .filter(|version| valid_data_dragon_version(version))
            else {
                return Ok(None);
            };
            if champion.is_empty() {
                return Ok(None);
            }
            let key = data_dragon_champion_key(champion, aliases);
            if key.is_empty() {
                return Ok(None);
            }
            let url =
                format!("https://ddragon.leagueoflegends.com/cdn/{version}/img/champion/{key}.png");
            check_string("gallery.icon.output_url", &url)?;
            Ok(Some(GalleryCardIcon {
                kind: "portrait".into(),
                url,
                label: if champion.is_empty() {
                    label.trim()
                } else {
                    champion
                }
                .into(),
            }))
        }
    }
}

fn player_summary_label(summary: &PlayerCardSummary) -> String {
    let champion = summary.champion_name.trim();
    if champion.is_empty() {
        String::new()
    } else {
        format!("{champion} | {}", player_summary_kda(summary))
    }
}

fn player_summary_kda(summary: &PlayerCardSummary) -> String {
    format!("{}/{}/{}", summary.kills, summary.deaths, summary.assists)
}

fn player_summary_stats_label(
    summary: &PlayerCardSummary,
    format: &GalleryCardTitleFormat,
) -> String {
    let mut parts = Vec::new();
    for stat in &format.stats {
        match stat {
            GalleryCardStat::Kda => parts.push(player_summary_kda(summary)),
            GalleryCardStat::CsPerMin { label } => {
                if let (Some(creep_score), Some(game_time_s)) =
                    (summary.creep_score, summary.game_time_s)
                {
                    if game_time_s > 0 {
                        let suffix = if label.trim().is_empty() {
                            "CS/min"
                        } else {
                            label.trim()
                        };
                        parts.push(format!(
                            "{:.1} {suffix}",
                            f64::from(creep_score) / (f64::from(game_time_s) / 60.0)
                        ));
                    }
                }
            }
        }
    }
    let separator = if format.separator.is_empty() {
        " | "
    } else {
        &format.separator
    };
    parts.join(separator)
}

fn play_summary_from_count(total: usize) -> String {
    match total {
        0 => "no submitted plays".into(),
        1 => "1 submitted play".into(),
        count => format!("{count} submitted plays"),
    }
}

fn summarize_plays(plays: &[GalleryPlay]) -> PlayOutcomeSummary {
    let passed = plays.iter().filter(|play| play.passed).count();
    let incomplete = plays
        .iter()
        .filter(|play| {
            !play.passed
                && !play.pp.is_some_and(|pp| pp.is_finite() && pp > 0.0)
                && !play.rank.trim().eq_ignore_ascii_case("F")
        })
        .count();
    let failed = plays.len().saturating_sub(passed + incomplete);
    PlayOutcomeSummary {
        total: plays.len(),
        passed,
        failed,
        incomplete,
    }
}

fn play_result_summary_from_counts(plays: PlayOutcomeSummary) -> String {
    let mut parts = Vec::new();
    if plays.passed != 0 {
        parts.push(format!(
            "{} {}",
            plays.passed,
            if plays.passed == 1 { "pass" } else { "passes" }
        ));
    }
    if plays.incomplete != 0 {
        parts.push(format!("{} incomplete", plays.incomplete));
    }
    if plays.failed != 0 {
        parts.push(format!(
            "{} {}",
            plays.failed,
            if plays.failed == 1 { "fail" } else { "fails" }
        ));
    }
    parts.join(" · ")
}

fn strip_media_extension(name: &str) -> String {
    for extension in [".mp4", ".mov", ".mkv", ".webm"] {
        if name
            .get(name.len().saturating_sub(extension.len())..)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case(extension))
        {
            let stem = name[..name.len() - extension.len()].trim();
            return if stem.is_empty() { name.trim() } else { stem }.to_owned();
        }
    }
    name.trim().to_owned()
}

fn data_dragon_champion_key(champion: &str, aliases: &[AssetAlias]) -> String {
    let lookup = alphanumeric_lookup(champion);
    if let Some(alias) = aliases
        .iter()
        .find(|alias| alphanumeric_lookup(&alias.alias) == lookup)
    {
        if alias
            .key
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
        {
            return alias.key.clone();
        }
    }
    champion
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut characters = part.chars();
            characters.next().map_or_else(String::new, |first| {
                let mut result = first.to_ascii_uppercase().to_string();
                result.push_str(characters.as_str());
                result
            })
        })
        .collect()
}

fn alphanumeric_lookup(value: &str) -> String {
    value
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect()
}

fn valid_data_dragon_version(version: &str) -> bool {
    let mut segments = version.split('.');
    (0..3).all(|_| {
        segments
            .next()
            .is_some_and(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
    }) && segments.next().is_none()
}

fn validate_card_input(
    input: &GalleryCardInput,
    presentation: &GalleryPresentation,
) -> Result<(), PresentationError> {
    check_string("gallery.name", &input.name)?;
    check_optional_string("gallery.title", input.title.as_deref())?;
    check_string("gallery.kind", &input.kind)?;
    check_string("gallery.fallback_title", &input.fallback_title)?;
    check_count("gallery.plays", input.plays.len(), MAX_GALLERY_CARD_PLAYS)?;
    if let Some(summary) = &input.player_summary {
        check_string("gallery.summary.champion_name", &summary.champion_name)?;
    }
    for play in &input.plays {
        check_string("gallery.play.rank", &play.rank)?;
        if play.pp.is_some_and(|pp| !pp.is_finite()) {
            return Err(PresentationError::Invalid {
                field: "gallery.play.pp",
            });
        }
    }
    validate_card_presentation(presentation)
}

fn validate_card_presentation(presentation: &GalleryPresentation) -> Result<(), PresentationError> {
    if let Some(format) = &presentation.card.title_format {
        check_string("gallery.card.separator", &format.separator)?;
        check_count(
            "gallery.card.stats",
            format.stats.len(),
            MAX_GALLERY_CARD_STATS,
        )?;
        for stat in &format.stats {
            if let GalleryCardStat::CsPerMin { label } = stat {
                check_string("gallery.card.stat.label", label)?;
            }
        }
    }
    if let Some(icon) = &presentation.card.icon {
        match icon {
            GalleryCardIconConfig::Asset { url, label } => {
                check_string("gallery.card.icon.url", url)?;
                check_string("gallery.card.icon.label", label)?;
            }
            GalleryCardIconConfig::Portrait { label, aliases } => {
                check_string("gallery.card.icon.label", label)?;
                check_count(
                    "gallery.card.icon.aliases",
                    aliases.len(),
                    MAX_PRESENTATION_ENTRIES,
                )?;
                for alias in aliases {
                    check_string("gallery.card.icon.alias", &alias.alias)?;
                    check_string("gallery.card.icon.alias_key", &alias.key)?;
                }
            }
        }
    }
    check_optional_string(
        "gallery.data_dragon_version",
        presentation.data_dragon_version.as_deref(),
    )?;
    validate_marker_inputs(&[], Some(&presentation.markers))
}

fn validate_key(field: &'static str, value: &str) -> Result<(), PresentationError> {
    let bytes = value.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 64
        || !bytes[0].is_ascii_alphabetic()
        || !bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        || matches!(value, "constructor" | "prototype")
    {
        Err(PresentationError::Invalid { field })
    } else {
        Ok(())
    }
}

fn check_optional_string(
    field: &'static str,
    value: Option<&str>,
) -> Result<(), PresentationError> {
    value.map_or(Ok(()), |value| check_string(field, value))
}

fn check_string(field: &'static str, value: &str) -> Result<(), PresentationError> {
    check_count(field, value.len(), MAX_CATALOG_STRING_BYTES)
}

fn check_count(
    field: &'static str,
    actual: usize,
    maximum: usize,
) -> Result<(), PresentationError> {
    if actual > maximum {
        Err(PresentationError::TooLarge {
            field,
            actual,
            maximum,
        })
    } else {
        Ok(())
    }
}
