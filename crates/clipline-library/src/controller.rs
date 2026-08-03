use std::collections::BTreeMap;
use std::sync::Arc;

use thiserror::Error;

use crate::{
    build_catalog_projection, CatalogAction, CatalogCloudPreferences,
    CatalogDialogAudioTrackProjection, CatalogDialogKind, CatalogDialogProjection, CatalogEffect,
    CatalogItemIdentity, CatalogLoadState, CatalogMenuProjection, CatalogOperationOwner,
    CatalogProjection, CatalogProjectionInput, CatalogProjectionSource, CatalogResult,
    CatalogRevision, CatalogSource, CatalogUploadOptions, CatalogUploadProjection,
    CatalogUploadVisibility, CatalogWarning, ClipDetailRequest, ClipPathIdentity,
    CloudCatalogOwner, CloudLibraryItem, CloudNextPage, CloudPageNumber, CloudPageOutcome,
    CloudReviewMediaOwner, CloudReviewMediaRequest, CloudThumbnailDescriptor, CloudThumbnailOwner,
    CloudThumbnailRequest, CloudWorkToken, DurableUploadToken, ForegroundGeneration,
    GalleryPresentation, GenerationError, LocalClipItem, LocalDayResolver, LocalGalleryOptions,
    LocalPageIndex, PayloadBoundsError, PosterStatus, PreparedCloudReviewMedia, PresentationError,
    ProjectionReservation, RemoteClipId, RequestGeneration, ResolvedLocalClip,
    SystemProjectionReservation, WindowAttachmentGeneration, WindowWorkToken,
    MAX_CATALOG_PAGE_ROWS, MAX_CATALOG_STRING_BYTES, MAX_LOCAL_INDEX_ROWS,
    MAX_POSTER_RESULT_ENTRIES, MAX_UPLOAD_SUMMARIES,
};

/// One update may issue one thumbnail request for every accepted Cloud page
/// row plus the four pre-existing lifecycle/refresh effects.
pub const MAX_CATALOG_EFFECTS_PER_UPDATE: usize = MAX_CATALOG_PAGE_ROWS + 4;
pub const MAX_CLOUD_THUMBNAIL_REQUESTS_PER_UPDATE: usize = MAX_CATALOG_PAGE_ROWS;
const _: () = assert!(MAX_CATALOG_EFFECTS_PER_UPDATE == 64);

type LocalIdentityLookup = Vec<(ClipPathIdentity, usize)>;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CatalogControllerError {
    #[error(transparent)]
    Generation(#[from] GenerationError),
    #[error(transparent)]
    Bounds(#[from] PayloadBoundsError),
    #[error(transparent)]
    Presentation(#[from] PresentationError),
    #[error("catalog controller rejected {field}")]
    Invalid { field: &'static str },
    #[error("catalog controller has no attached window")]
    Detached,
    #[error("catalog controller has no current Cloud account owner")]
    NoCloudOwner,
    #[error("could not reserve bounded controller storage for {field}")]
    Allocation { field: &'static str },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WindowOwner {
    attachment: WindowAttachmentGeneration,
    foreground: ForegroundGeneration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalRefreshTarget {
    revision: CatalogRevision,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CloudRefreshTarget {
    owner: CloudCatalogOwner,
    revision: CatalogRevision,
    page: CloudPageNumber,
    query: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IssuedLocalRefresh {
    token: WindowWorkToken,
    target: LocalRefreshTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IssuedCloudRefresh {
    token: CloudWorkToken,
    target: CloudRefreshTarget,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct LocalRefreshLane {
    in_flight: Option<IssuedLocalRefresh>,
    dirty: Option<LocalRefreshTarget>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CloudRefreshLane {
    in_flight: Option<IssuedCloudRefresh>,
    dirty: Option<CloudRefreshTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RefreshResultMatch {
    NotRefresh,
    Stale,
    Current(CatalogOperationOwner),
}

#[derive(Debug, Clone, PartialEq)]
struct AcceptedCloudPage {
    owner: CloudCatalogOwner,
    query: String,
    page: CloudPageNumber,
    items: Arc<Vec<CloudLibraryItem>>,
    has_next: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingMutation {
    RenameTitle {
        token: WindowWorkToken,
        target: ClipPathIdentity,
    },
    RenameFile {
        token: WindowWorkToken,
        target: ClipPathIdentity,
    },
    Delete {
        token: WindowWorkToken,
        targets: Arc<Vec<ClipPathIdentity>>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct CatalogControllerState {
    pub revision: CatalogRevision,
    pub source: CatalogSource,
    pub selection_mode: bool,
    pub selected: Arc<Vec<CatalogItemIdentity>>,
    pub active: Option<CatalogItemIdentity>,
    pub menu: Option<CatalogMenuProjection>,
    pub dialog: Option<Arc<CatalogDialogProjection>>,
    pub local_items: Arc<Vec<LocalClipItem>>,
    pub local_warnings: Arc<Vec<CatalogWarning>>,
    pub cloud_warnings: Arc<Vec<CatalogWarning>>,
    pub local_truncated: bool,
    pub projection: Arc<CatalogProjection>,
    window: Option<WindowOwner>,
    next_request: RequestGeneration,
    cloud_owner: Option<CloudCatalogOwner>,
    cloud_preferences: CatalogCloudPreferences,
    cloud_page: Option<Arc<AcceptedCloudPage>>,
    local_options: LocalGalleryOptions,
    local_page: LocalPageIndex,
    cloud_query: String,
    cloud_target_page: CloudPageNumber,
    local_load_state: CatalogLoadState,
    cloud_load_state: CatalogLoadState,
    local_lookup: Arc<Vec<(ClipPathIdentity, usize)>>,
    posters: Arc<BTreeMap<ClipPathIdentity, PosterStatus>>,
    poster_order: Arc<Vec<ClipPathIdentity>>,
    cloud_posters: Arc<BTreeMap<CloudThumbnailDescriptor, PosterStatus>>,
    cloud_poster_order: Arc<Vec<CloudThumbnailDescriptor>>,
    pending_cloud_thumbnails: Arc<BTreeMap<CloudThumbnailDescriptor, CloudThumbnailOwner>>,
    uploads: Arc<Vec<CatalogUploadProjection>>,
    local_refresh: LocalRefreshLane,
    cloud_refresh: CloudRefreshLane,
    pending_detail: Option<ClipDetailRequest>,
    pending_mutation: Option<PendingMutation>,
    dialog_delete_targets: Option<Arc<Vec<CatalogItemIdentity>>>,
    active_local_target: Option<ResolvedLocalClip>,
    pending_cloud_review: Option<CloudReviewMediaOwner>,
    active_cloud_media: Option<PreparedCloudReviewMedia>,
}

pub struct CatalogController {
    state: CatalogControllerState,
    days: Arc<dyn LocalDayResolver>,
    reservation: Arc<dyn ProjectionReservation>,
    gallery: GalleryPresentation,
}

impl CatalogController {
    pub fn new(days: Arc<dyn LocalDayResolver>) -> Result<Self, CatalogControllerError> {
        Self::with_reservation(days, Arc::new(SystemProjectionReservation))
    }

    pub fn with_reservation(
        days: Arc<dyn LocalDayResolver>,
        reservation: Arc<dyn ProjectionReservation>,
    ) -> Result<Self, CatalogControllerError> {
        let empty_projection = build_catalog_projection(
            &CatalogProjectionInput {
                revision: CatalogRevision::INITIAL,
                source: CatalogProjectionSource::Local {
                    items: &[],
                    options: &LocalGalleryOptions::default(),
                    page: LocalPageIndex::new(0)?,
                },
                gallery: &GalleryPresentation::default(),
                selected: &[],
                selection_mode: false,
                cloud_query: "",
                active: None,
                posters: &BTreeMap::new(),
                cloud_posters: &BTreeMap::new(),
                menu: None,
                dialog: None,
                uploads: &[],
                load_state: CatalogLoadState::Empty,
            },
            days.as_ref(),
            reservation.as_ref(),
        )?;
        Ok(Self {
            state: CatalogControllerState {
                revision: CatalogRevision::INITIAL,
                source: CatalogSource::Local,
                selection_mode: false,
                selected: Arc::new(Vec::new()),
                active: None,
                menu: None,
                dialog: None,
                local_items: Arc::new(Vec::new()),
                local_warnings: Arc::new(Vec::new()),
                cloud_warnings: Arc::new(Vec::new()),
                local_truncated: false,
                projection: Arc::new(empty_projection),
                window: None,
                next_request: RequestGeneration::INITIAL,
                cloud_owner: None,
                cloud_preferences: CatalogCloudPreferences::default(),
                cloud_page: None,
                local_options: LocalGalleryOptions::default(),
                local_page: LocalPageIndex::new(0)?,
                cloud_query: String::new(),
                cloud_target_page: CloudPageNumber::new(1)?,
                local_load_state: CatalogLoadState::Empty,
                cloud_load_state: CatalogLoadState::Disconnected,
                local_lookup: Arc::new(Vec::new()),
                posters: Arc::new(BTreeMap::new()),
                poster_order: Arc::new(Vec::new()),
                cloud_posters: Arc::new(BTreeMap::new()),
                cloud_poster_order: Arc::new(Vec::new()),
                pending_cloud_thumbnails: Arc::new(BTreeMap::new()),
                uploads: Arc::new(Vec::new()),
                local_refresh: LocalRefreshLane::default(),
                cloud_refresh: CloudRefreshLane::default(),
                pending_detail: None,
                pending_mutation: None,
                dialog_delete_targets: None,
                active_local_target: None,
                pending_cloud_review: None,
                active_cloud_media: None,
            },
            days,
            reservation,
            gallery: GalleryPresentation::default(),
        })
    }

    #[must_use]
    pub const fn state(&self) -> &CatalogControllerState {
        &self.state
    }

    pub fn attach(
        &mut self,
        attachment: WindowAttachmentGeneration,
        foreground: ForegroundGeneration,
    ) -> Result<Vec<CatalogEffect>, CatalogControllerError> {
        let mut candidate = self.state.clone();
        if candidate.window
            == Some(WindowOwner {
                attachment,
                foreground,
            })
        {
            return Ok(Vec::new());
        }
        if let Some(issued) = candidate.local_refresh.in_flight.take() {
            candidate.local_refresh.dirty = Some(issued.target);
        }
        if let Some(issued) = candidate.cloud_refresh.in_flight.take() {
            candidate.cloud_refresh.dirty = Some(issued.target);
        }
        if candidate.pending_mutation.is_some() {
            candidate.local_refresh.dirty = Some(LocalRefreshTarget {
                revision: next_revision(&mut candidate)?,
            });
        }
        candidate.window = Some(WindowOwner {
            attachment,
            foreground,
        });
        candidate.menu = None;
        candidate.dialog = None;
        candidate.pending_detail = None;
        candidate.pending_mutation = None;
        candidate.dialog_delete_targets = None;
        candidate.pending_cloud_review = None;
        candidate.pending_cloud_thumbnails = Arc::new(BTreeMap::new());
        let mut effects = effect_buffer(self.reservation.as_ref())?;
        let local_target = if let Some(dirty) = candidate.local_refresh.dirty.take() {
            Some(dirty)
        } else if candidate.source == CatalogSource::Local && candidate.local_items.is_empty() {
            Some(LocalRefreshTarget {
                revision: next_revision(&mut candidate)?,
            })
        } else {
            None
        };
        if let Some(target) = local_target {
            issue_local_refresh(&mut candidate, target, &mut effects)?;
        }
        if let Some(owner) = candidate.cloud_owner.clone() {
            let cloud_target = if let Some(dirty) = candidate.cloud_refresh.dirty.take() {
                Some(dirty)
            } else if candidate.source == CatalogSource::Cloud && candidate.cloud_page.is_none() {
                Some(CloudRefreshTarget {
                    owner,
                    revision: next_revision(&mut candidate)?,
                    page: candidate.cloud_target_page,
                    query: candidate.cloud_query.clone(),
                })
            } else {
                None
            };
            if let Some(target) = cloud_target {
                issue_cloud_refresh(&mut candidate, target, &mut effects)?;
            }
        }
        if candidate.source == CatalogSource::Cloud
            && candidate.cloud_refresh.in_flight.is_none()
            && candidate.cloud_page.is_some()
        {
            let owner = candidate
                .cloud_owner
                .clone()
                .ok_or(CatalogControllerError::NoCloudOwner)?;
            let token = CloudWorkToken {
                window: next_window_token(&mut candidate)?,
                account_key: owner.account_key,
                account_generation: owner.account_generation,
            };
            issue_cloud_thumbnail_requests(
                &mut candidate,
                token,
                &mut effects,
                self.reservation.as_ref(),
            )?;
        }
        if let Some(item) = candidate
            .active
            .clone()
            .filter(|item| item.source() == CatalogSource::Local)
        {
            if let Some(target) = candidate
                .active_local_target
                .clone()
                .filter(|target| item.local_path() == Some(&target.identity))
            {
                effects.push(CatalogEffect::OpenLocalReview {
                    token: next_window_token(&mut candidate)?,
                    target,
                });
            }
        }
        if let Some(item) = candidate
            .active
            .clone()
            .filter(|item| item.source() == CatalogSource::Cloud)
        {
            if let Some(owner) = candidate.cloud_owner.clone() {
                if !item.matches_cloud_catalog_owner(&owner) {
                    return Err(CatalogControllerError::Invalid {
                        field: "active.cloud_owner",
                    });
                }
                let window = next_window_token(&mut candidate)?;
                let token = CloudWorkToken {
                    window,
                    account_key: owner.account_key,
                    account_generation: owner.account_generation,
                };
                let cloud_item = resolve_cloud_item(&candidate, &item)?;
                let request = cloud_review_request(token, item, cloud_item)?;
                candidate.pending_cloud_review = Some(request.owner.clone());
                effects.push(CatalogEffect::PrepareCloudReviewMedia { request });
            }
        }
        self.commit(candidate, effects)
    }

    pub fn detach(&mut self) -> Result<Vec<CatalogEffect>, CatalogControllerError> {
        let mut candidate = self.state.clone();
        let close_token = if candidate.active.is_some() {
            Some(next_window_token(&mut candidate)?)
        } else {
            None
        };
        candidate.window = None;
        if let Some(issued) = candidate.local_refresh.in_flight.take() {
            candidate.local_refresh.dirty = Some(issued.target);
        }
        if let Some(issued) = candidate.cloud_refresh.in_flight.take() {
            candidate.cloud_refresh.dirty = Some(issued.target);
        }
        if candidate.pending_mutation.is_some() {
            candidate.local_refresh.dirty = Some(LocalRefreshTarget {
                revision: next_revision(&mut candidate)?,
            });
        }
        candidate.pending_detail = None;
        candidate.pending_mutation = None;
        candidate.dialog_delete_targets = None;
        candidate.pending_cloud_review = None;
        candidate.pending_cloud_thumbnails = Arc::new(BTreeMap::new());
        candidate.menu = None;
        candidate.dialog = None;
        let mut effects = Vec::new();
        if let Some(token) = close_token {
            effects.push(CatalogEffect::CloseReview { token });
        }
        if let Some(media) = candidate.active_cloud_media.take() {
            effects.push(CatalogEffect::ReleaseCloudReviewMedia {
                lease_id: media.lease_id,
            });
        }
        self.commit(candidate, effects)
    }

    pub fn set_cloud_owner(
        &mut self,
        owner: Option<CloudCatalogOwner>,
    ) -> Result<Vec<CatalogEffect>, CatalogControllerError> {
        self.set_cloud_context(owner, CatalogCloudPreferences::default())
    }

    pub fn set_cloud_context(
        &mut self,
        owner: Option<CloudCatalogOwner>,
        preferences: CatalogCloudPreferences,
    ) -> Result<Vec<CatalogEffect>, CatalogControllerError> {
        if self.state.cloud_owner == owner && self.state.cloud_preferences == preferences {
            return Ok(Vec::new());
        }
        let mut candidate = self.state.clone();
        candidate.cloud_owner = owner.clone();
        candidate.cloud_preferences = preferences;
        candidate.cloud_page = None;
        candidate.cloud_warnings = Arc::new(Vec::new());
        candidate.cloud_refresh = CloudRefreshLane::default();
        candidate.cloud_posters = Arc::new(BTreeMap::new());
        candidate.cloud_poster_order = Arc::new(Vec::new());
        candidate.pending_cloud_thumbnails = Arc::new(BTreeMap::new());
        candidate.cloud_target_page = CloudPageNumber::new(1)?;
        candidate.cloud_load_state = if owner.is_some() {
            CatalogLoadState::Empty
        } else {
            CatalogLoadState::Disconnected
        };
        candidate.menu = None;
        candidate.dialog = None;
        candidate.pending_detail = None;
        candidate.dialog_delete_targets = None;
        candidate.pending_cloud_review = None;
        let mut effects = Vec::new();
        if let Some(media) = candidate.active_cloud_media.take() {
            effects.push(CatalogEffect::ReleaseCloudReviewMedia {
                lease_id: media.lease_id,
            });
        }
        if candidate
            .active
            .as_ref()
            .is_some_and(|item| item.source() == CatalogSource::Cloud)
        {
            candidate.active = None;
            if candidate.window.is_some() {
                effects.push(CatalogEffect::CloseReview {
                    token: next_window_token(&mut candidate)?,
                });
            }
        }
        if candidate.source == CatalogSource::Cloud {
            if let Some(owner) = owner {
                let target = CloudRefreshTarget {
                    owner,
                    revision: next_revision(&mut candidate)?,
                    page: candidate.cloud_target_page,
                    query: candidate.cloud_query.clone(),
                };
                request_cloud_refresh(&mut candidate, target, &mut effects)?;
            } else {
                candidate.revision = candidate.revision.checked_next()?;
            }
        } else {
            candidate.revision = candidate.revision.checked_next()?;
        }
        self.commit(candidate, effects)
    }

    pub fn dispatch(
        &mut self,
        action: CatalogAction,
    ) -> Result<Vec<CatalogEffect>, CatalogControllerError> {
        action.validate_bounds()?;
        let mut candidate = self.state.clone();
        let mut effects = effect_buffer(self.reservation.as_ref())?;
        self.reduce_action(&mut candidate, action, &mut effects)?;
        self.commit(candidate, effects)
    }

    pub fn accept(
        &mut self,
        result: CatalogResult,
    ) -> Result<Vec<CatalogEffect>, CatalogControllerError> {
        let refresh_match = match_refresh_result(&self.state, &result);
        if matches!(&refresh_match, RefreshResultMatch::Stale) {
            return Ok(Vec::new());
        }
        let prepared_lease = match &result {
            CatalogResult::CloudReviewMediaPrepared { owner, media } => {
                if result.validate_bounds().is_err()
                    || self.state.pending_cloud_review.as_ref() != Some(owner)
                {
                    return Ok(cloud_media_release(media.lease_id));
                }
                Some(media.lease_id)
            }
            _ => {
                if let Err(error) = result.validate_bounds() {
                    if let RefreshResultMatch::Current(owner) = refresh_match.clone() {
                        return self.recover_invalid_refresh(owner);
                    }
                    return Err(error.into());
                }
                None
            }
        };
        let mut candidate = self.state.clone();
        let mut effects = match effect_buffer(self.reservation.as_ref()) {
            Ok(effects) => effects,
            Err(_) if prepared_lease.is_some() => {
                return Ok(cloud_media_release(prepared_lease.expect("guarded above")));
            }
            Err(error) => {
                if let RefreshResultMatch::Current(owner) = refresh_match {
                    return self.recover_invalid_refresh(owner);
                }
                return Err(error);
            }
        };
        let accepted = match self.reduce_result(&mut candidate, result, &mut effects) {
            Ok(accepted) => accepted,
            Err(_) if prepared_lease.is_some() => {
                return Ok(cloud_media_release(prepared_lease.expect("guarded above")));
            }
            Err(error) => {
                if let RefreshResultMatch::Current(owner) = refresh_match {
                    return self.recover_invalid_refresh(owner);
                }
                return Err(error);
            }
        };
        if !accepted {
            return Ok(Vec::new());
        }
        match self.commit(candidate, effects) {
            Ok(effects) => Ok(effects),
            Err(_) if prepared_lease.is_some() => {
                Ok(cloud_media_release(prepared_lease.expect("guarded above")))
            }
            Err(error) => Err(error),
        }
    }

    fn recover_invalid_refresh(
        &mut self,
        owner: CatalogOperationOwner,
    ) -> Result<Vec<CatalogEffect>, CatalogControllerError> {
        let mut candidate = self.state.clone();
        let mut effects = effect_buffer(self.reservation.as_ref())?;
        if !apply_operation_failure(
            &mut candidate,
            owner,
            "invalid refresh completion".to_owned(),
            &mut effects,
        )? {
            return Ok(Vec::new());
        }
        self.commit(candidate, effects)
    }

    fn reduce_action(
        &self,
        state: &mut CatalogControllerState,
        action: CatalogAction,
        effects: &mut Vec<CatalogEffect>,
    ) -> Result<(), CatalogControllerError> {
        match action {
            CatalogAction::Refresh => self.refresh_current(state, effects)?,
            CatalogAction::SetSource { source } => {
                if state.source == source {
                    return Ok(());
                }
                state.source = source;
                state.menu = None;
                state.dialog = None;
                state.pending_detail = None;
                state.dialog_delete_targets = None;
                state.pending_cloud_review = None;
                state.selection_mode = false;
                state.selected = Arc::new(Vec::new());
                if state
                    .active
                    .as_ref()
                    .is_some_and(|item| item.source() != source)
                {
                    state.active = None;
                    state.active_local_target = None;
                    effects.push(CatalogEffect::CloseReview {
                        token: next_window_token(state)?,
                    });
                    if let Some(media) = state.active_cloud_media.take() {
                        effects.push(CatalogEffect::ReleaseCloudReviewMedia {
                            lease_id: media.lease_id,
                        });
                    }
                }
                state.revision = state.revision.checked_next()?;
                if source == CatalogSource::Cloud {
                    if let Some(owner) = state.cloud_owner.clone() {
                        request_cloud_refresh(
                            state,
                            CloudRefreshTarget {
                                owner,
                                revision: state.revision,
                                page: state.cloud_target_page,
                                query: state.cloud_query.clone(),
                            },
                            effects,
                        )?;
                    }
                }
            }
            CatalogAction::SetQuery { query } => {
                state.revision = state.revision.checked_next()?;
                match state.source {
                    CatalogSource::Local => {
                        state.local_options.query = query;
                        state.local_page = LocalPageIndex::new(0)?;
                    }
                    CatalogSource::Cloud => {
                        state.cloud_query = query;
                        state.cloud_target_page = CloudPageNumber::new(1)?;
                        if let Some(owner) = state.cloud_owner.clone() {
                            request_cloud_refresh(
                                state,
                                CloudRefreshTarget {
                                    owner,
                                    revision: state.revision,
                                    page: state.cloud_target_page,
                                    query: state.cloud_query.clone(),
                                },
                                effects,
                            )?;
                        }
                    }
                }
            }
            CatalogAction::SetLocalFilter { filter } => {
                require_local_source(state)?;
                state.local_options.filter = filter;
                reset_local_page_and_bump(state)?;
            }
            CatalogAction::SetLocalSort { sort } => {
                require_local_source(state)?;
                state.local_options.sort = sort;
                reset_local_page_and_bump(state)?;
            }
            CatalogAction::SetLocalGrouping { grouping } => {
                require_local_source(state)?;
                state.local_options.grouping = grouping;
                reset_local_page_and_bump(state)?;
            }
            CatalogAction::SetLocalPage { page } => {
                require_local_source(state)?;
                let maximum = state
                    .projection
                    .page
                    .page_count
                    .unwrap_or(1)
                    .saturating_sub(1);
                state.local_page = LocalPageIndex::new(page.get().min(maximum))?;
                state.revision = state.revision.checked_next()?;
            }
            CatalogAction::PreviousPage => previous_page(state, effects)?,
            CatalogAction::NextPage => next_page(state, effects)?,
            CatalogAction::EnterSelection => {
                require_local_source(state)?;
                state.selection_mode = true;
                state.revision = state.revision.checked_next()?;
            }
            CatalogAction::ExitSelection => {
                state.selection_mode = false;
                state.selected = Arc::new(Vec::new());
                state.revision = state.revision.checked_next()?;
            }
            CatalogAction::ToggleSelection { item } => {
                require_local_identity(state, &item)?;
                let mut selected = reserve_clone(
                    self.reservation.as_ref(),
                    "controller.selected",
                    state.selected.as_slice(),
                    usize::from(state.selected.binary_search(&item).is_err()),
                )?;
                match selected.binary_search(&item) {
                    Ok(index) => {
                        selected.remove(index);
                    }
                    Err(index) => {
                        if selected.len() >= MAX_LOCAL_INDEX_ROWS {
                            return Err(CatalogControllerError::Invalid {
                                field: "selection.capacity",
                            });
                        }
                        selected.insert(index, item);
                    }
                }
                state.selected = Arc::new(selected);
                state.selection_mode = true;
                state.revision = state.revision.checked_next()?;
            }
            CatalogAction::SelectVisiblePage => {
                require_local_source(state)?;
                let additional = state.projection.rows.len();
                let mut selected = reserve_clone(
                    self.reservation.as_ref(),
                    "controller.selected",
                    state.selected.as_slice(),
                    additional,
                )?;
                for row in &state.projection.rows {
                    if let Err(index) = selected.binary_search(&row.identity) {
                        if selected.len() >= MAX_LOCAL_INDEX_ROWS {
                            return Err(CatalogControllerError::Invalid {
                                field: "selection.capacity",
                            });
                        }
                        selected.insert(index, row.identity.clone());
                    }
                }
                state.selected = Arc::new(selected);
                state.selection_mode = true;
                state.revision = state.revision.checked_next()?;
            }
            CatalogAction::ClearSelection => {
                state.selected = Arc::new(Vec::new());
                state.revision = state.revision.checked_next()?;
            }
            CatalogAction::OpenItem { item } => {
                state.revision = state.revision.checked_next()?;
                match item.source() {
                    CatalogSource::Local => {
                        require_local_identity(state, &item)?;
                        let token = next_window_token(state)?;
                        let target = resolve_local(state, &item)?;
                        effects.push(CatalogEffect::OpenLocalReview {
                            token,
                            target: target.clone(),
                        });
                        state.active_local_target = Some(target);
                        state.active = Some(item);
                    }
                    CatalogSource::Cloud => {
                        let (token, cloud_item) = resolve_cloud(state, &item)?;
                        let request = cloud_review_request(token, item.clone(), cloud_item)?;
                        state.pending_cloud_review = Some(request.owner.clone());
                        effects.push(CatalogEffect::PrepareCloudReviewMedia { request });
                    }
                }
                state.menu = None;
            }
            CatalogAction::CloseActive => close_active(state, effects)?,
            CatalogAction::OpenContext { item } => {
                require_current_identity(state, &item)?;
                state.menu = Some(menu_for(item));
                state.revision = state.revision.checked_next()?;
            }
            CatalogAction::CloseContext => {
                state.menu = None;
                state.revision = state.revision.checked_next()?;
            }
            CatalogAction::OpenRenameTitle { item } => {
                open_local_text_dialog(state, item, CatalogDialogKind::RenameTitle)?;
            }
            CatalogAction::OpenRenameFile { item } => {
                open_local_text_dialog(state, item, CatalogDialogKind::RenameFile)?;
            }
            CatalogAction::OpenDelete { item } => open_delete_dialog(state, vec![item])?,
            CatalogAction::OpenDeleteSelection => {
                if state.selected.is_empty() {
                    return Err(CatalogControllerError::Invalid {
                        field: "delete.selection_empty",
                    });
                }
                let selected = reserve_clone(
                    self.reservation.as_ref(),
                    "controller.delete_dialog_targets",
                    state.selected.as_slice(),
                    0,
                )?;
                open_delete_dialog(state, selected)?;
            }
            CatalogAction::OpenUpload { item } => {
                let target = resolve_local(state, &item)?;
                let local = resolve_local_item(state, &item)?;
                let title = local
                    .title
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .unwrap_or(local.name.trim())
                    .to_owned();
                let token = next_window_token(state)?;
                let request = ClipDetailRequest::new(target.identity.clone(), token);
                state.dialog_delete_targets = None;
                state.pending_detail = Some(request.clone());
                effects.push(CatalogEffect::LoadClipDetail {
                    token,
                    request,
                    target,
                    title,
                    description: String::new(),
                });
                state.revision = state.revision.checked_next()?;
            }
            CatalogAction::OpenCancelUpload { token } => {
                open_cancel_upload_dialog(state, token)?;
            }
            CatalogAction::SetDialogText { field, value } => {
                let mut dialog = clone_dialog_for_update(state, self.reservation.as_ref())?;
                match field {
                    crate::CatalogDialogTextField::Title
                    | crate::CatalogDialogTextField::FileName => dialog.text_value = Some(value),
                    crate::CatalogDialogTextField::Description => dialog.description = Some(value),
                }
                state.dialog = Some(Arc::new(dialog));
                state.revision = state.revision.checked_next()?;
            }
            CatalogAction::SetUploadVisibility { visibility } => {
                let mut dialog = clone_dialog_for_update(state, self.reservation.as_ref())?;
                if dialog.kind != CatalogDialogKind::Upload {
                    return Err(CatalogControllerError::Invalid {
                        field: "dialog.kind",
                    });
                }
                dialog.visibility = Some(visibility);
                state.dialog = Some(Arc::new(dialog));
                state.revision = state.revision.checked_next()?;
            }
            CatalogAction::SetUploadAudioTrack { track_id, selected } => {
                let mut dialog = clone_dialog_for_update(state, self.reservation.as_ref())?;
                let track = dialog
                    .audio_tracks
                    .iter_mut()
                    .find(|track| track.id == track_id)
                    .ok_or(CatalogControllerError::Invalid {
                        field: "dialog.audio_track",
                    })?;
                track.selected = selected;
                state.dialog = Some(Arc::new(dialog));
                state.revision = state.revision.checked_next()?;
            }
            CatalogAction::SetDeleteLocalAfterUpload { enabled } => {
                let mut dialog = clone_dialog_for_update(state, self.reservation.as_ref())?;
                if dialog.kind != CatalogDialogKind::Upload {
                    return Err(CatalogControllerError::Invalid {
                        field: "dialog.kind",
                    });
                }
                dialog.delete_local_after_upload = enabled;
                state.dialog = Some(Arc::new(dialog));
                state.revision = state.revision.checked_next()?;
            }
            CatalogAction::ConfirmDialog => {
                confirm_dialog(state, effects, self.reservation.as_ref())?;
            }
            CatalogAction::CancelDialog => {
                state.dialog = None;
                state.pending_detail = None;
                state.dialog_delete_targets = None;
                state.revision = state.revision.checked_next()?;
            }
            CatalogAction::Reveal { item } => {
                let token = next_window_token(state)?;
                effects.push(CatalogEffect::Reveal {
                    token,
                    target: resolve_local(state, &item)?,
                });
            }
            CatalogAction::OpenInBrowser { item } => {
                let (token, cloud) = resolve_cloud(state, &item)?;
                effects.push(CatalogEffect::OpenInBrowser {
                    token,
                    item,
                    url: cloud.remote_url.clone(),
                });
            }
            CatalogAction::CopyPublicLink { item } => {
                let (token, cloud) = resolve_cloud(state, &item)?;
                effects.push(CatalogEffect::CopyPublicLink {
                    token,
                    item,
                    url: cloud.remote_url.clone(),
                });
            }
            CatalogAction::CancelUpload { token } => {
                effects.push(CatalogEffect::CancelUpload { token });
            }
            CatalogAction::Escape => escape(state, effects)?,
        }
        Ok(())
    }

    fn reduce_result(
        &self,
        state: &mut CatalogControllerState,
        result: CatalogResult,
        effects: &mut Vec<CatalogEffect>,
    ) -> Result<bool, CatalogControllerError> {
        match result {
            CatalogResult::LocalIndex(completion) => {
                let Some(issued) = state.local_refresh.in_flight.as_ref() else {
                    return Ok(false);
                };
                if issued.token != completion.token || issued.target.revision != completion.revision
                {
                    return Ok(false);
                }
                if let Some(dirty) = state.local_refresh.dirty.take() {
                    state.local_refresh.in_flight = None;
                    issue_local_refresh(state, dirty, effects)?;
                    return Ok(true);
                }
                let (items, lookup) = self.stage_local_index(completion.items)?;
                state.local_items = Arc::new(items);
                state.local_warnings = Arc::new(completion.warnings);
                state.local_lookup = Arc::new(lookup);
                state.local_truncated = completion.truncated;
                state.local_refresh.in_flight = None;
                state.local_load_state = CatalogLoadState::Ready;
                state.revision = state.revision.checked_next()?;
                if !completion.truncated {
                    prune_local_identity_state(state, self.reservation.as_ref())?;
                }
                Ok(true)
            }
            CatalogResult::CloudPage(completion) => {
                let Some(issued) = state.cloud_refresh.in_flight.as_ref() else {
                    return Ok(false);
                };
                if issued.token != completion.token
                    || issued.target.revision != completion.revision
                    || CloudCatalogOwner::from_work_token(&completion.token) != issued.target.owner
                {
                    return Ok(false);
                }
                if let Some(dirty) = state.cloud_refresh.dirty.take() {
                    state.cloud_refresh.in_flight = None;
                    issue_cloud_refresh(state, dirty, effects)?;
                    return Ok(true);
                }
                match completion.outcome {
                    CloudPageOutcome::Page { page, items, next } => {
                        if page != issued.target.page {
                            return Err(CatalogControllerError::Invalid {
                                field: "cloud.page_mismatch",
                            });
                        }
                        state.cloud_page = Some(Arc::new(AcceptedCloudPage {
                            owner: issued.target.owner.clone(),
                            query: issued.target.query.clone(),
                            page,
                            items: Arc::new(items),
                            has_next: matches!(next, CloudNextPage::Probe { .. }),
                        }));
                        state.cloud_target_page = page;
                        issue_cloud_thumbnail_requests(
                            state,
                            completion.token.clone(),
                            effects,
                            self.reservation.as_ref(),
                        )?;
                    }
                    CloudPageOutcome::PastEnd {
                        requested_page,
                        fallback_page,
                    } => {
                        let Some(current) = state.cloud_page.as_ref() else {
                            return Err(CatalogControllerError::Invalid {
                                field: "cloud.past_end_without_page",
                            });
                        };
                        if requested_page != issued.target.page
                            || fallback_page != current.page
                            || current.owner != issued.target.owner
                            || current.query != issued.target.query
                        {
                            return Err(CatalogControllerError::Invalid {
                                field: "cloud.past_end_fallback",
                            });
                        }
                        let mut retained = current.as_ref().clone();
                        retained.has_next = false;
                        state.cloud_page = Some(Arc::new(retained));
                        state.cloud_target_page = fallback_page;
                    }
                }
                state.cloud_warnings = Arc::new(completion.warnings);
                state.cloud_refresh.in_flight = None;
                state.cloud_load_state = CatalogLoadState::Ready;
                state.revision = state.revision.checked_next()?;
                Ok(true)
            }
            CatalogResult::ClipDetail(result) => {
                let Some(request) = state.pending_detail.as_ref() else {
                    return Ok(false);
                };
                if !result.matches_request(request) {
                    return Ok(false);
                }
                let item = CatalogItemIdentity::Local {
                    path: request.item().clone(),
                };
                require_local_identity(state, &item)?;
                let detail = result.detail();
                let mut audio_tracks = Vec::new();
                reserve_vec(
                    self.reservation.as_ref(),
                    &mut audio_tracks,
                    "controller.dialog_audio_tracks",
                    detail.audio_tracks().len(),
                )?;
                audio_tracks.extend(detail.audio_tracks().iter().map(|track| {
                    CatalogDialogAudioTrackProjection {
                        id: track.id().to_owned(),
                        label: track.label().to_owned(),
                        selected: true,
                    }
                }));
                state.dialog = Some(Arc::new(CatalogDialogProjection {
                    kind: CatalogDialogKind::Upload,
                    target: item,
                    title: "Upload clip".to_owned(),
                    message: detail.upload().audio_summary().to_owned(),
                    confirm_label: "Upload".to_owned(),
                    destructive: false,
                    text_value: Some(detail.upload().title().to_owned()),
                    description: Some(detail.upload().description().to_owned()),
                    visibility: Some(state.cloud_preferences.default_visibility),
                    audio_tracks,
                    delete_local_after_upload: state.cloud_preferences.delete_local_after_upload,
                    cancel_upload_token: None,
                    progress: None,
                }));
                state.dialog_delete_targets = None;
                state.pending_detail = None;
                state.revision = state.revision.checked_next()?;
                Ok(true)
            }
            CatalogResult::OperationFailed { owner, message } => {
                apply_operation_failure(state, owner, message, effects)
            }
            CatalogResult::CloudReviewMediaPrepared { owner, media } => {
                if state.pending_cloud_review.as_ref() != Some(&owner) {
                    effects.push(CatalogEffect::ReleaseCloudReviewMedia {
                        lease_id: media.lease_id,
                    });
                    return Ok(true);
                }
                effects.push(CatalogEffect::OpenPreparedCloudReview {
                    owner: owner.clone(),
                    media: media.clone(),
                });
                state.active = Some(owner.item.clone());
                state.active_local_target = None;
                state.pending_cloud_review = None;
                if let Some(previous) = state.active_cloud_media.replace(media) {
                    effects.push(CatalogEffect::ReleaseCloudReviewMedia {
                        lease_id: previous.lease_id,
                    });
                }
                state.revision = state.revision.checked_next()?;
                Ok(true)
            }
            CatalogResult::CloudThumbnail { owner, status } => {
                let Some(expected) = state.pending_cloud_thumbnails.get(&owner.descriptor) else {
                    return Ok(false);
                };
                if expected != &owner || !cloud_thumbnail_is_current(state, &owner.descriptor) {
                    return Ok(false);
                }
                let mut pending = reserve_map_clone(
                    self.reservation.as_ref(),
                    "controller.pending_cloud_thumbnails_result",
                    state.pending_cloud_thumbnails.as_ref(),
                )?;
                if !matches!(&status, PosterStatus::Queued) {
                    pending.remove(&owner.descriptor);
                }
                let mut posters = reserve_map_clone(
                    self.reservation.as_ref(),
                    "controller.cloud_posters_result",
                    state.cloud_posters.as_ref(),
                )?;
                posters.insert(owner.descriptor, status);
                state.pending_cloud_thumbnails = Arc::new(pending);
                state.cloud_posters = Arc::new(posters);
                state.revision = state.revision.checked_next()?;
                Ok(true)
            }
            CatalogResult::Poster { token, poster } => {
                let Some(window) = state.window else {
                    return Ok(false);
                };
                if token.window.attachment != window.attachment
                    || token.window.foreground != window.foreground
                {
                    return Ok(false);
                }
                if state
                    .local_lookup
                    .binary_search_by(|candidate| candidate.0.cmp(&poster.path))
                    .is_err()
                {
                    return Ok(false);
                }
                let mut posters = reserve_map_clone(
                    self.reservation.as_ref(),
                    "controller.posters",
                    state.posters.as_ref(),
                )?;
                let mut poster_order = reserve_clone(
                    self.reservation.as_ref(),
                    "controller.poster_order",
                    state.poster_order.as_slice(),
                    usize::from(!posters.contains_key(&poster.path)),
                )?;
                if !posters.contains_key(&poster.path) {
                    if poster_order.len() >= MAX_POSTER_RESULT_ENTRIES {
                        let oldest = poster_order.remove(0);
                        posters.remove(&oldest);
                    }
                    poster_order.push(poster.path.clone());
                }
                posters.insert(poster.path, poster.status);
                state.posters = Arc::new(posters);
                state.poster_order = Arc::new(poster_order);
                state.revision = state.revision.checked_next()?;
                Ok(true)
            }
            CatalogResult::UploadByteProgress { token, progress } => {
                apply_upload_result(state, token, progress, false, self.reservation.as_ref())
            }
            CatalogResult::UploadCompleted { token, result } => {
                apply_upload_result(state, token, result, true, self.reservation.as_ref())
            }
            CatalogResult::RenameCompleted { token, result } => {
                let Some(pending) = state.pending_mutation.as_ref() else {
                    return Ok(false);
                };
                let expected = match pending {
                    PendingMutation::RenameTitle {
                        token: expected,
                        target,
                    }
                    | PendingMutation::RenameFile {
                        token: expected,
                        target,
                    } if *expected == token => target.clone(),
                    _ => return Ok(false),
                };
                let old = ClipPathIdentity::from_text(&result.old_path).ok_or(
                    CatalogControllerError::Invalid {
                        field: "rename.old_identity",
                    },
                )?;
                if old != expected {
                    return Err(CatalogControllerError::Invalid {
                        field: "rename.expected_identity",
                    });
                }
                apply_rename(state, expected, result, self.reservation.as_ref())?;
                state.pending_mutation = None;
                state.dialog = None;
                state.dialog_delete_targets = None;
                state.revision = state.revision.checked_next()?;
                Ok(true)
            }
            CatalogResult::DeleteCompleted { token, report } => {
                let Some(PendingMutation::Delete {
                    token: expected,
                    targets,
                }) = state.pending_mutation.as_ref()
                else {
                    return Ok(false);
                };
                if *expected != token {
                    return Ok(false);
                }
                let targets = targets.clone();
                let active_deleted = state.active.as_ref().is_some_and(|active| {
                    active.local_path().is_some_and(|path| {
                        report.deleted.iter().any(|deleted| {
                            ClipPathIdentity::from_text(deleted).as_ref() == Some(path)
                        })
                    })
                });
                apply_delete_report(
                    state,
                    targets.as_slice(),
                    &report,
                    self.reservation.as_ref(),
                )?;
                state.pending_mutation = None;
                state.dialog = partial_delete_dialog(targets.as_slice(), &report)?;
                state.dialog_delete_targets = None;
                state.revision = state.revision.checked_next()?;
                if active_deleted {
                    effects.push(CatalogEffect::CloseReview {
                        token: next_window_token(state)?,
                    });
                }
                let target = LocalRefreshTarget {
                    revision: state.revision,
                };
                request_local_refresh(state, target, effects)?;
                Ok(true)
            }
            CatalogResult::ForegroundFeedback { token, .. } => {
                Ok(state.window.is_some_and(|window| {
                    token.attachment == window.attachment && token.foreground == window.foreground
                }))
            }
        }
    }

    fn refresh_current(
        &self,
        state: &mut CatalogControllerState,
        effects: &mut Vec<CatalogEffect>,
    ) -> Result<(), CatalogControllerError> {
        let revision = next_revision(state)?;
        match state.source {
            CatalogSource::Local => {
                request_local_refresh(state, LocalRefreshTarget { revision }, effects)
            }
            CatalogSource::Cloud => {
                if let Some(owner) = state.cloud_owner.clone() {
                    request_cloud_refresh(
                        state,
                        CloudRefreshTarget {
                            owner,
                            revision,
                            page: state.cloud_target_page,
                            query: state.cloud_query.clone(),
                        },
                        effects,
                    )
                } else {
                    Ok(())
                }
            }
        }
    }

    fn stage_local_index(
        &self,
        items: Vec<LocalClipItem>,
    ) -> Result<(Vec<LocalClipItem>, LocalIdentityLookup), CatalogControllerError> {
        if items.len() > MAX_LOCAL_INDEX_ROWS {
            return Err(CatalogControllerError::Invalid {
                field: "local_index.capacity",
            });
        }
        self.reservation
            .before_reserve("controller.local_index", items.len())?;
        let mut lookup = Vec::new();
        reserve_vec(
            self.reservation.as_ref(),
            &mut lookup,
            "controller.local_lookup",
            items.len(),
        )?;
        for (index, item) in items.iter().enumerate() {
            let path = item
                .path_identity()
                .ok_or(CatalogControllerError::Invalid {
                    field: "local_index.identity",
                })?;
            lookup.push((path, index));
        }
        lookup.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        if lookup.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err(CatalogControllerError::Invalid {
                field: "local_index.duplicate_identity",
            });
        }
        Ok((items, lookup))
    }

    fn commit(
        &mut self,
        mut candidate: CatalogControllerState,
        effects: Vec<CatalogEffect>,
    ) -> Result<Vec<CatalogEffect>, CatalogControllerError> {
        if effects.len() > MAX_CATALOG_EFFECTS_PER_UPDATE {
            return Err(CatalogControllerError::Invalid {
                field: "effects.capacity",
            });
        }
        for effect in &effects {
            effect.validate_for_cloud_owner(candidate.cloud_owner.as_ref())?;
        }
        candidate.projection = Arc::new(build_projection(
            &candidate,
            &self.gallery,
            self.days.as_ref(),
            self.reservation.as_ref(),
        )?);
        self.state = candidate;
        Ok(effects)
    }
}

fn match_refresh_result(
    state: &CatalogControllerState,
    result: &CatalogResult,
) -> RefreshResultMatch {
    match result {
        CatalogResult::LocalIndex(completion) => {
            let Some(issued) = state.local_refresh.in_flight.as_ref() else {
                return RefreshResultMatch::Stale;
            };
            if issued.token != completion.token || issued.target.revision != completion.revision {
                return RefreshResultMatch::Stale;
            }
            RefreshResultMatch::Current(CatalogOperationOwner::LocalRefresh {
                token: completion.token,
                revision: completion.revision,
            })
        }
        CatalogResult::CloudPage(completion) => {
            let Some(issued) = state.cloud_refresh.in_flight.as_ref() else {
                return RefreshResultMatch::Stale;
            };
            if issued.token != completion.token
                || issued.target.revision != completion.revision
                || CloudCatalogOwner::from_work_token(&completion.token) != issued.target.owner
            {
                return RefreshResultMatch::Stale;
            }
            RefreshResultMatch::Current(CatalogOperationOwner::CloudRefresh {
                token: completion.token.clone(),
                revision: completion.revision,
                page: issued.target.page,
            })
        }
        _ => RefreshResultMatch::NotRefresh,
    }
}

fn cloud_media_release(lease_id: crate::CloudMediaLeaseId) -> Vec<CatalogEffect> {
    Vec::from([CatalogEffect::ReleaseCloudReviewMedia { lease_id }])
}

fn build_projection(
    state: &CatalogControllerState,
    gallery: &GalleryPresentation,
    days: &dyn LocalDayResolver,
    reservation: &dyn ProjectionReservation,
) -> Result<CatalogProjection, PresentationError> {
    let input = match state.source {
        CatalogSource::Local => CatalogProjectionInput {
            revision: state.revision,
            source: CatalogProjectionSource::Local {
                items: state.local_items.as_slice(),
                options: &state.local_options,
                page: state.local_page,
            },
            gallery,
            selected: state.selected.as_slice(),
            selection_mode: state.selection_mode,
            cloud_query: &state.cloud_query,
            active: state.active.as_ref(),
            posters: state.posters.as_ref(),
            cloud_posters: state.cloud_posters.as_ref(),
            menu: state.menu.as_ref(),
            dialog: state.dialog.as_deref(),
            uploads: state.uploads.as_slice(),
            load_state: state.local_load_state.clone(),
        },
        CatalogSource::Cloud => {
            let source = if let Some(owner) = state.cloud_owner.as_ref() {
                let (page, items, has_next) = state
                    .cloud_page
                    .as_ref()
                    .map_or((state.cloud_target_page, &[][..], false), |page| {
                        (page.page, page.items.as_slice(), page.has_next)
                    });
                CatalogProjectionSource::Cloud {
                    owner,
                    page,
                    items,
                    has_next,
                }
            } else {
                CatalogProjectionSource::CloudDisconnected
            };
            CatalogProjectionInput {
                revision: state.revision,
                source,
                gallery,
                selected: &[],
                selection_mode: false,
                cloud_query: &state.cloud_query,
                active: state.active.as_ref(),
                posters: state.posters.as_ref(),
                cloud_posters: state.cloud_posters.as_ref(),
                menu: state.menu.as_ref(),
                dialog: state.dialog.as_deref(),
                uploads: state.uploads.as_slice(),
                load_state: state.cloud_load_state.clone(),
            }
        }
    };
    build_catalog_projection(&input, days, reservation)
}

fn effect_buffer(
    reservation: &dyn ProjectionReservation,
) -> Result<Vec<CatalogEffect>, CatalogControllerError> {
    let mut effects = Vec::new();
    reserve_vec(
        reservation,
        &mut effects,
        "controller.effects",
        MAX_CATALOG_EFFECTS_PER_UPDATE,
    )?;
    Ok(effects)
}

fn next_revision(state: &mut CatalogControllerState) -> Result<CatalogRevision, GenerationError> {
    let revision = state.revision.checked_next()?;
    state.revision = revision;
    Ok(revision)
}

fn next_window_token(
    state: &mut CatalogControllerState,
) -> Result<WindowWorkToken, CatalogControllerError> {
    let window = state.window.ok_or(CatalogControllerError::Detached)?;
    let request = state.next_request.checked_next()?;
    state.next_request = request;
    Ok(WindowWorkToken {
        attachment: window.attachment,
        foreground: window.foreground,
        request,
    })
}

fn request_local_refresh(
    state: &mut CatalogControllerState,
    target: LocalRefreshTarget,
    effects: &mut Vec<CatalogEffect>,
) -> Result<(), CatalogControllerError> {
    state.local_load_state = if state.local_items.is_empty() {
        CatalogLoadState::Loading
    } else {
        state.local_load_state.clone()
    };
    if state.local_refresh.in_flight.is_some() || state.window.is_none() {
        state.local_refresh.dirty = Some(target);
        return Ok(());
    }
    issue_local_refresh(state, target, effects)
}

fn issue_local_refresh(
    state: &mut CatalogControllerState,
    target: LocalRefreshTarget,
    effects: &mut Vec<CatalogEffect>,
) -> Result<(), CatalogControllerError> {
    let token = next_window_token(state)?;
    effects.push(CatalogEffect::RefreshLocal {
        token,
        revision: target.revision,
    });
    state.local_refresh.in_flight = Some(IssuedLocalRefresh { token, target });
    Ok(())
}

fn request_cloud_refresh(
    state: &mut CatalogControllerState,
    target: CloudRefreshTarget,
    effects: &mut Vec<CatalogEffect>,
) -> Result<(), CatalogControllerError> {
    state.cloud_load_state = if state.cloud_page.is_none() {
        CatalogLoadState::Loading
    } else {
        state.cloud_load_state.clone()
    };
    if state.cloud_refresh.in_flight.is_some() || state.window.is_none() {
        state.cloud_refresh.dirty = Some(target);
        return Ok(());
    }
    issue_cloud_refresh(state, target, effects)
}

fn issue_cloud_refresh(
    state: &mut CatalogControllerState,
    target: CloudRefreshTarget,
    effects: &mut Vec<CatalogEffect>,
) -> Result<(), CatalogControllerError> {
    let window = next_window_token(state)?;
    let token = CloudWorkToken {
        window,
        account_key: target.owner.account_key.clone(),
        account_generation: target.owner.account_generation,
    };
    effects.push(CatalogEffect::RefreshCloud {
        token: token.clone(),
        revision: target.revision,
        page: target.page,
        query: target.query.clone(),
    });
    state.cloud_refresh.in_flight = Some(IssuedCloudRefresh { token, target });
    Ok(())
}

fn reset_local_page_and_bump(state: &mut CatalogControllerState) -> Result<(), GenerationError> {
    state.local_page = LocalPageIndex::new(0).expect("zero local page is always valid");
    state.revision = state.revision.checked_next()?;
    Ok(())
}

fn require_local_source(state: &CatalogControllerState) -> Result<(), CatalogControllerError> {
    if state.source == CatalogSource::Local {
        Ok(())
    } else {
        Err(CatalogControllerError::Invalid {
            field: "source.local",
        })
    }
}

fn require_local_identity(
    state: &CatalogControllerState,
    item: &CatalogItemIdentity,
) -> Result<(), CatalogControllerError> {
    let Some(path) = item.local_path() else {
        return Err(CatalogControllerError::Invalid {
            field: "identity.local",
        });
    };
    if state
        .local_lookup
        .binary_search_by(|candidate| candidate.0.cmp(path))
        .is_err()
    {
        return Err(CatalogControllerError::Invalid {
            field: "identity.unresolved",
        });
    }
    Ok(())
}

fn require_current_identity(
    state: &CatalogControllerState,
    item: &CatalogItemIdentity,
) -> Result<(), CatalogControllerError> {
    match state.source {
        CatalogSource::Local => require_local_identity(state, item),
        CatalogSource::Cloud => resolve_cloud_item(state, item).map(|_| ()),
    }
}

fn resolve_local(
    state: &CatalogControllerState,
    item: &CatalogItemIdentity,
) -> Result<ResolvedLocalClip, CatalogControllerError> {
    let path = item.local_path().ok_or(CatalogControllerError::Invalid {
        field: "identity.local",
    })?;
    let index = state
        .local_lookup
        .binary_search_by(|candidate| candidate.0.cmp(path))
        .map_err(|_| CatalogControllerError::Invalid {
            field: "identity.unresolved",
        })?;
    let item = &state.local_items[state.local_lookup[index].1];
    Ok(ResolvedLocalClip::with_file_identity(
        path.clone(),
        item.path.clone(),
        item.file_identity,
    )?)
}

fn resolve_local_item<'a>(
    state: &'a CatalogControllerState,
    item: &CatalogItemIdentity,
) -> Result<&'a LocalClipItem, CatalogControllerError> {
    let path = item.local_path().ok_or(CatalogControllerError::Invalid {
        field: "identity.local",
    })?;
    let index = state
        .local_lookup
        .binary_search_by(|candidate| candidate.0.cmp(path))
        .map_err(|_| CatalogControllerError::Invalid {
            field: "identity.unresolved",
        })?;
    Ok(&state.local_items[state.local_lookup[index].1])
}

fn resolve_cloud<'a>(
    state: &'a mut CatalogControllerState,
    item: &CatalogItemIdentity,
) -> Result<(CloudWorkToken, &'a CloudLibraryItem), CatalogControllerError> {
    let window = next_window_token(state)?;
    let owner = state
        .cloud_owner
        .clone()
        .ok_or(CatalogControllerError::NoCloudOwner)?;
    if !item.matches_cloud_catalog_owner(&owner) {
        return Err(CatalogControllerError::Invalid {
            field: "identity.cloud_owner",
        });
    }
    let cloud = resolve_cloud_item(state, item)?;
    Ok((
        CloudWorkToken {
            window,
            account_key: owner.account_key,
            account_generation: owner.account_generation,
        },
        cloud,
    ))
}

fn resolve_cloud_item<'a>(
    state: &'a CatalogControllerState,
    item: &CatalogItemIdentity,
) -> Result<&'a CloudLibraryItem, CatalogControllerError> {
    let (account_key, account_generation, remote_clip_id) = match item {
        CatalogItemIdentity::Cloud {
            account_key,
            account_generation,
            remote_clip_id,
        } => (account_key, account_generation, remote_clip_id),
        CatalogItemIdentity::Local { .. } => {
            return Err(CatalogControllerError::Invalid {
                field: "identity.cloud",
            });
        }
    };
    let owner = state
        .cloud_owner
        .as_ref()
        .ok_or(CatalogControllerError::NoCloudOwner)?;
    if account_key != &owner.account_key || *account_generation != owner.account_generation {
        return Err(CatalogControllerError::Invalid {
            field: "identity.cloud_owner",
        });
    }
    state
        .cloud_page
        .as_ref()
        .and_then(|page| {
            page.items
                .iter()
                .find(|cloud| cloud.remote_clip_id == remote_clip_id.as_str())
        })
        .ok_or(CatalogControllerError::Invalid {
            field: "identity.unresolved",
        })
}

fn cloud_review_request(
    token: CloudWorkToken,
    identity: CatalogItemIdentity,
    item: &CloudLibraryItem,
) -> Result<CloudReviewMediaRequest, CatalogControllerError> {
    let expected_size_bytes = item
        .file_size_bytes
        .map(u64::try_from)
        .transpose()
        .map_err(|_| CatalogControllerError::Invalid {
            field: "cloud.file_size_bytes",
        })?;
    Ok(CloudReviewMediaRequest::new(
        CloudReviewMediaOwner::new(token, identity)?,
        item.updated_at_unix,
        expected_size_bytes,
    )?)
}

fn cloud_thumbnail_descriptor(
    owner: &CloudCatalogOwner,
    item: &CloudLibraryItem,
) -> Result<CloudThumbnailDescriptor, CatalogControllerError> {
    let identity = CatalogItemIdentity::Cloud {
        account_key: owner.account_key.clone(),
        account_generation: owner.account_generation,
        remote_clip_id: RemoteClipId::new(item.remote_clip_id.clone()).map_err(|_| {
            CatalogControllerError::Invalid {
                field: "cloud_thumbnail.remote_clip_id",
            }
        })?,
    };
    Ok(CloudThumbnailDescriptor::new(
        identity,
        item.updated_at_unix,
    )?)
}

fn issue_cloud_thumbnail_requests(
    state: &mut CatalogControllerState,
    token: CloudWorkToken,
    effects: &mut Vec<CatalogEffect>,
    reservation: &dyn ProjectionReservation,
) -> Result<(), CatalogControllerError> {
    let page = state
        .cloud_page
        .clone()
        .ok_or(CatalogControllerError::Invalid {
            field: "cloud_thumbnail.page",
        })?;
    if page.items.len() > MAX_CLOUD_THUMBNAIL_REQUESTS_PER_UPDATE {
        return Err(CatalogControllerError::Invalid {
            field: "cloud_thumbnail.effect_capacity",
        });
    }
    if CloudCatalogOwner::from_work_token(&token) != page.owner {
        return Err(CatalogControllerError::Invalid {
            field: "cloud_thumbnail.owner",
        });
    }
    reservation.before_reserve("controller.pending_cloud_thumbnails", page.items.len())?;
    let mut pending = BTreeMap::new();
    let mut posters = reserve_map_clone(
        reservation,
        "controller.cloud_posters",
        state.cloud_posters.as_ref(),
    )?;
    let mut order = reserve_clone(
        reservation,
        "controller.cloud_poster_order",
        state.cloud_poster_order.as_slice(),
        page.items.len(),
    )?;
    for item in page.items.iter() {
        let descriptor = cloud_thumbnail_descriptor(&page.owner, item)?;
        let owner = CloudThumbnailOwner::new(token.clone(), descriptor.clone())?;
        let request = CloudThumbnailRequest::new(owner.clone())?;
        if !posters.contains_key(&descriptor) {
            if order.len() >= MAX_POSTER_RESULT_ENTRIES {
                let oldest = order.remove(0);
                posters.remove(&oldest);
            }
            order.push(descriptor.clone());
            posters.insert(descriptor.clone(), PosterStatus::Queued);
        }
        pending.insert(descriptor, owner);
        effects.push(CatalogEffect::LoadCloudThumbnail { request });
    }
    state.pending_cloud_thumbnails = Arc::new(pending);
    state.cloud_posters = Arc::new(posters);
    state.cloud_poster_order = Arc::new(order);
    Ok(())
}

fn cloud_thumbnail_is_current(
    state: &CatalogControllerState,
    descriptor: &CloudThumbnailDescriptor,
) -> bool {
    let Some(page) = state.cloud_page.as_ref() else {
        return false;
    };
    if !descriptor.item.matches_cloud_catalog_owner(&page.owner) {
        return false;
    }
    let CatalogItemIdentity::Cloud { remote_clip_id, .. } = &descriptor.item else {
        return false;
    };
    page.items.iter().any(|item| {
        item.remote_clip_id == remote_clip_id.as_str() && item.updated_at_unix == descriptor.version
    })
}

fn menu_for(target: CatalogItemIdentity) -> CatalogMenuProjection {
    let local = target.source() == CatalogSource::Local;
    CatalogMenuProjection {
        target,
        can_review: true,
        can_rename: local,
        can_delete: local,
        can_upload: local,
        can_reveal: local,
        can_open_browser: !local,
        can_copy_link: !local,
    }
}

fn open_local_text_dialog(
    state: &mut CatalogControllerState,
    item: CatalogItemIdentity,
    kind: CatalogDialogKind,
) -> Result<(), CatalogControllerError> {
    let target = resolve_local(state, &item)?;
    let row = state
        .projection
        .rows
        .iter()
        .find(|row| row.identity == item);
    let text = match kind {
        CatalogDialogKind::RenameTitle => row.map(|row| row.title.clone()).unwrap_or_default(),
        CatalogDialogKind::RenameFile => target
            .path
            .rsplit(['\\', '/'])
            .next()
            .unwrap_or(&target.path)
            .to_owned(),
        _ => {
            return Err(CatalogControllerError::Invalid {
                field: "dialog.rename_kind",
            });
        }
    };
    state.dialog = Some(Arc::new(CatalogDialogProjection {
        kind,
        target: item,
        title: match kind {
            CatalogDialogKind::RenameTitle => "Rename title",
            CatalogDialogKind::RenameFile => "Rename file",
            _ => unreachable!(),
        }
        .to_owned(),
        message: String::new(),
        confirm_label: "Save".to_owned(),
        destructive: false,
        text_value: Some(text),
        description: None,
        visibility: None,
        audio_tracks: Vec::new(),
        delete_local_after_upload: false,
        cancel_upload_token: None,
        progress: None,
    }));
    state.dialog_delete_targets = None;
    state.menu = None;
    state.revision = state.revision.checked_next()?;
    Ok(())
}

fn open_delete_dialog(
    state: &mut CatalogControllerState,
    items: Vec<CatalogItemIdentity>,
) -> Result<(), CatalogControllerError> {
    if items.is_empty() {
        return Err(CatalogControllerError::Invalid {
            field: "delete.targets",
        });
    }
    for item in &items {
        require_local_identity(state, item)?;
    }
    let target = items[0].clone();
    let item_count = items.len();
    state.dialog = Some(Arc::new(CatalogDialogProjection {
        kind: CatalogDialogKind::Delete,
        target,
        title: "Delete clip".to_owned(),
        message: if item_count == 1 {
            "Delete this clip permanently?".to_owned()
        } else {
            format!("Delete {item_count} clips permanently?")
        },
        confirm_label: "Delete".to_owned(),
        destructive: true,
        text_value: None,
        description: None,
        visibility: None,
        audio_tracks: Vec::new(),
        delete_local_after_upload: false,
        cancel_upload_token: None,
        progress: None,
    }));
    state.dialog_delete_targets = Some(Arc::new(items));
    state.menu = None;
    state.revision = state.revision.checked_next()?;
    Ok(())
}

fn open_cancel_upload_dialog(
    state: &mut CatalogControllerState,
    token: DurableUploadToken,
) -> Result<(), CatalogControllerError> {
    let upload = state
        .uploads
        .iter()
        .find(|upload| upload.token == token)
        .ok_or(CatalogControllerError::Invalid {
            field: "upload.cancel_token",
        })?;
    let target = CatalogItemIdentity::Local {
        path: token.source_path.clone(),
    };
    require_local_identity(state, &target)?;
    state.dialog = Some(Arc::new(CatalogDialogProjection {
        kind: CatalogDialogKind::CancelUpload,
        target,
        title: "Cancel upload".to_owned(),
        message: "Stop uploading this clip? The local clip will be kept.".to_owned(),
        confirm_label: "Cancel upload".to_owned(),
        destructive: true,
        text_value: None,
        description: None,
        visibility: None,
        audio_tracks: Vec::new(),
        delete_local_after_upload: false,
        cancel_upload_token: Some(token),
        progress: Some(format_upload_progress(&upload.summary)),
    }));
    state.dialog_delete_targets = None;
    state.menu = None;
    state.revision = state.revision.checked_next()?;
    Ok(())
}

fn partial_delete_dialog(
    targets: &[ClipPathIdentity],
    report: &crate::DeletedClipsReport,
) -> Result<Option<Arc<CatalogDialogProjection>>, CatalogControllerError> {
    let Some((failed_path, _)) = report.failed.first() else {
        return Ok(None);
    };
    let failed_identity =
        ClipPathIdentity::from_text(failed_path).ok_or(CatalogControllerError::Invalid {
            field: "delete.failed_identity",
        })?;
    if !targets.contains(&failed_identity) {
        return Err(CatalogControllerError::Invalid {
            field: "delete.failed_target",
        });
    }
    let message = format!(
        "Deleted {} of {} clips. {} could not be deleted and remain in the Library.",
        report.deleted.len(),
        report.deleted.len().saturating_add(report.failed.len()),
        report.failed.len(),
    );
    if message.len() > MAX_CATALOG_STRING_BYTES {
        return Err(CatalogControllerError::Invalid {
            field: "delete.partial_report",
        });
    }
    Ok(Some(Arc::new(CatalogDialogProjection {
        kind: CatalogDialogKind::PartialDelete,
        target: CatalogItemIdentity::Local {
            path: failed_identity,
        },
        title: "Some clips were not deleted".to_owned(),
        message,
        confirm_label: "Close".to_owned(),
        destructive: false,
        text_value: None,
        description: None,
        visibility: None,
        audio_tracks: Vec::new(),
        delete_local_after_upload: false,
        cancel_upload_token: None,
        progress: None,
    })))
}

fn format_upload_progress(summary: &crate::UploadSummary) -> String {
    if summary.file_size_bytes == 0 {
        return summary.upload_status.clone();
    }
    let percent = summary
        .received_size_bytes
        .min(summary.file_size_bytes)
        .saturating_mul(100)
        / summary.file_size_bytes;
    format!("{} — {percent}%", summary.upload_status)
}

fn apply_upload_result(
    state: &mut CatalogControllerState,
    token: DurableUploadToken,
    summary: crate::UploadSummary,
    completed: bool,
    reservation: &dyn ProjectionReservation,
) -> Result<bool, CatalogControllerError> {
    let progress = format_upload_progress(&summary);
    let entry = CatalogUploadProjection::new(token, summary)?;
    let mut uploads = reserve_clone(
        reservation,
        "controller.uploads",
        state.uploads.as_slice(),
        1,
    )?;
    if let Some(existing) = uploads.iter_mut().find(|item| item.token == entry.token) {
        *existing = entry.clone();
    } else if let Some(existing) = uploads
        .iter_mut()
        .find(|item| item.token.source_path == entry.token.source_path)
    {
        *existing = entry.clone();
    } else {
        if uploads.len() >= MAX_UPLOAD_SUMMARIES {
            uploads.remove(0);
        }
        uploads.push(entry.clone());
    }
    if let Some(dialog) = state.dialog.as_ref().filter(|dialog| {
        dialog.kind == CatalogDialogKind::CancelUpload
            && dialog
                .cancel_upload_token
                .as_ref()
                .is_some_and(|pending| pending.source_path == entry.token.source_path)
    }) {
        if completed || dialog.cancel_upload_token.as_ref() != Some(&entry.token) {
            state.dialog = None;
        } else {
            let mut updated = dialog.as_ref().clone();
            updated.progress = Some(progress);
            state.dialog = Some(Arc::new(updated));
        }
    }
    state.uploads = Arc::new(uploads);
    state.revision = state.revision.checked_next()?;
    Ok(true)
}

fn clone_dialog_for_update(
    state: &CatalogControllerState,
    reservation: &dyn ProjectionReservation,
) -> Result<CatalogDialogProjection, CatalogControllerError> {
    let dialog = state
        .dialog
        .as_deref()
        .ok_or(CatalogControllerError::Invalid {
            field: "dialog.missing",
        })?;
    let mut audio_tracks = Vec::new();
    reserve_vec(
        reservation,
        &mut audio_tracks,
        "controller.dialog_audio_tracks",
        dialog.audio_tracks.len(),
    )?;
    audio_tracks.extend(dialog.audio_tracks.iter().cloned());
    Ok(CatalogDialogProjection {
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
        cancel_upload_token: dialog.cancel_upload_token.clone(),
        progress: dialog.progress.clone(),
    })
}

fn confirm_dialog(
    state: &mut CatalogControllerState,
    effects: &mut Vec<CatalogEffect>,
    reservation: &dyn ProjectionReservation,
) -> Result<(), CatalogControllerError> {
    if state.pending_mutation.is_some() {
        return Err(CatalogControllerError::Invalid {
            field: "mutation.pending",
        });
    }
    let dialog = clone_dialog_for_update(state, reservation)?;
    let token = next_window_token(state)?;
    match dialog.kind {
        CatalogDialogKind::RenameTitle => {
            let target = resolve_local(state, &dialog.target)?;
            effects.push(CatalogEffect::RenameTitle {
                token,
                target: target.clone(),
                title: dialog.text_value.unwrap_or_default(),
            });
            state.pending_mutation = Some(PendingMutation::RenameTitle {
                token,
                target: target.identity,
            });
        }
        CatalogDialogKind::RenameFile => {
            let target = resolve_local(state, &dialog.target)?;
            effects.push(CatalogEffect::RenameFile {
                token,
                target: target.clone(),
                file_name: dialog.text_value.unwrap_or_default(),
            });
            state.pending_mutation = Some(PendingMutation::RenameFile {
                token,
                target: target.identity,
            });
        }
        CatalogDialogKind::Delete => {
            let selected =
                state
                    .dialog_delete_targets
                    .as_ref()
                    .ok_or(CatalogControllerError::Invalid {
                        field: "delete.dialog_targets",
                    })?;
            let mut targets = Vec::new();
            reserve_vec(
                reservation,
                &mut targets,
                "controller.delete_targets",
                selected.len(),
            )?;
            let mut identities = Vec::new();
            reserve_vec(
                reservation,
                &mut identities,
                "controller.delete_identities",
                selected.len(),
            )?;
            for item in selected.iter() {
                let target = resolve_local(state, item)?;
                identities.push(target.identity.clone());
                targets.push(target);
            }
            effects.push(CatalogEffect::Delete { token, targets });
            state.pending_mutation = Some(PendingMutation::Delete {
                token,
                targets: Arc::new(identities),
            });
        }
        CatalogDialogKind::Upload => {
            let target = resolve_local(state, &dialog.target)?;
            let owner = state
                .cloud_owner
                .clone()
                .ok_or(CatalogControllerError::NoCloudOwner)?;
            let selected_track_count = dialog
                .audio_tracks
                .iter()
                .filter(|track| track.selected)
                .count();
            let mut audio_track_ids = Vec::new();
            reserve_vec(
                reservation,
                &mut audio_track_ids,
                "controller.upload_audio_tracks",
                selected_track_count,
            )?;
            audio_track_ids.extend(
                dialog
                    .audio_tracks
                    .iter()
                    .filter(|track| track.selected)
                    .map(|track| track.id.clone()),
            );
            effects.push(CatalogEffect::StartUpload {
                token,
                owner,
                target,
                options: CatalogUploadOptions {
                    title: dialog.text_value,
                    description: dialog.description,
                    visibility: dialog
                        .visibility
                        .unwrap_or(CatalogUploadVisibility::Private),
                    audio_track_ids,
                    delete_local_after_upload: dialog.delete_local_after_upload,
                },
            });
            state.dialog = None;
            state.dialog_delete_targets = None;
        }
        CatalogDialogKind::CancelUpload => {
            let token = dialog
                .cancel_upload_token
                .ok_or(CatalogControllerError::Invalid {
                    field: "dialog.cancel_upload_token",
                })?;
            effects.push(CatalogEffect::CancelUpload { token });
            state.dialog = None;
            state.dialog_delete_targets = None;
        }
        CatalogDialogKind::PartialDelete => {
            state.dialog = None;
            state.dialog_delete_targets = None;
        }
    }
    state.revision = state.revision.checked_next()?;
    Ok(())
}

fn close_active(
    state: &mut CatalogControllerState,
    effects: &mut Vec<CatalogEffect>,
) -> Result<(), CatalogControllerError> {
    let canceled_pending = state.pending_cloud_review.take().is_some();
    if state.active.take().is_some() {
        state.active_local_target = None;
        effects.push(CatalogEffect::CloseReview {
            token: next_window_token(state)?,
        });
        if let Some(media) = state.active_cloud_media.take() {
            effects.push(CatalogEffect::ReleaseCloudReviewMedia {
                lease_id: media.lease_id,
            });
        }
        state.revision = state.revision.checked_next()?;
    } else if canceled_pending {
        state.revision = state.revision.checked_next()?;
    }
    Ok(())
}

fn apply_operation_failure(
    state: &mut CatalogControllerState,
    owner: CatalogOperationOwner,
    message: String,
    effects: &mut Vec<CatalogEffect>,
) -> Result<bool, CatalogControllerError> {
    match owner {
        CatalogOperationOwner::LocalRefresh { token, revision } => {
            let Some(issued) = state.local_refresh.in_flight.as_ref() else {
                return Ok(false);
            };
            if issued.token != token || issued.target.revision != revision {
                return Ok(false);
            }
            state.local_refresh.in_flight = None;
            if let Some(dirty) = state.local_refresh.dirty.take() {
                issue_local_refresh(state, dirty, effects)?;
            } else {
                state.local_load_state = CatalogLoadState::Error { message };
                state.revision = state.revision.checked_next()?;
            }
            Ok(true)
        }
        CatalogOperationOwner::CloudRefresh {
            token,
            revision,
            page,
        } => {
            let Some(issued) = state.cloud_refresh.in_flight.as_ref() else {
                return Ok(false);
            };
            if issued.token != token
                || issued.target.revision != revision
                || issued.target.page != page
            {
                return Ok(false);
            }
            state.cloud_refresh.in_flight = None;
            if let Some(dirty) = state.cloud_refresh.dirty.take() {
                issue_cloud_refresh(state, dirty, effects)?;
            } else {
                state.cloud_load_state = CatalogLoadState::Error { message };
                state.revision = state.revision.checked_next()?;
            }
            Ok(true)
        }
        CatalogOperationOwner::ClipDetail { owner } => {
            if state.pending_detail.as_ref().map(ClipDetailRequest::owner) != Some(&owner) {
                return Ok(false);
            }
            state.pending_detail = None;
            state.dialog = None;
            state.revision = state.revision.checked_next()?;
            Ok(true)
        }
        CatalogOperationOwner::CloudReviewMedia { owner } => {
            if state.pending_cloud_review.as_ref() != Some(&owner) {
                return Ok(false);
            }
            state.pending_cloud_review = None;
            state.revision = state.revision.checked_next()?;
            Ok(true)
        }
        CatalogOperationOwner::RenameTitle { token, target } => {
            if !matches!(
                state.pending_mutation.as_ref(),
                Some(PendingMutation::RenameTitle {
                    token: expected_token,
                    target: expected_target,
                }) if *expected_token == token && *expected_target == target
            ) {
                return Ok(false);
            }
            state.pending_mutation = None;
            state.dialog = None;
            state.dialog_delete_targets = None;
            state.revision = state.revision.checked_next()?;
            Ok(true)
        }
        CatalogOperationOwner::RenameFile { token, target } => {
            if !matches!(
                state.pending_mutation.as_ref(),
                Some(PendingMutation::RenameFile {
                    token: expected_token,
                    target: expected_target,
                }) if *expected_token == token && *expected_target == target
            ) {
                return Ok(false);
            }
            state.pending_mutation = None;
            state.dialog = None;
            state.dialog_delete_targets = None;
            state.revision = state.revision.checked_next()?;
            Ok(true)
        }
        CatalogOperationOwner::Delete { token, targets } => {
            if !matches!(
                state.pending_mutation.as_ref(),
                Some(PendingMutation::Delete {
                    token: expected_token,
                    targets: expected_targets,
                }) if *expected_token == token && expected_targets.as_slice() == targets
            ) {
                return Ok(false);
            }
            state.pending_mutation = None;
            state.dialog = None;
            state.dialog_delete_targets = None;
            state.revision = state.revision.checked_next()?;
            Ok(true)
        }
    }
}

fn escape(
    state: &mut CatalogControllerState,
    effects: &mut Vec<CatalogEffect>,
) -> Result<(), CatalogControllerError> {
    if state.dialog.take().is_some() {
        state.pending_detail = None;
        state.dialog_delete_targets = None;
        state.revision = state.revision.checked_next()?;
    } else if state.menu.take().is_some() {
        state.revision = state.revision.checked_next()?;
    } else if !state.selected.is_empty() {
        state.selected = Arc::new(Vec::new());
        state.revision = state.revision.checked_next()?;
    } else if state.selection_mode {
        state.selection_mode = false;
        state.revision = state.revision.checked_next()?;
    } else {
        close_active(state, effects)?;
    }
    Ok(())
}

fn previous_page(
    state: &mut CatalogControllerState,
    effects: &mut Vec<CatalogEffect>,
) -> Result<(), CatalogControllerError> {
    if !state.projection.page.has_previous {
        return Ok(());
    }
    state.revision = state.revision.checked_next()?;
    match state.source {
        CatalogSource::Local => {
            let previous = state.local_page.get().saturating_sub(1);
            state.local_page = LocalPageIndex::new(previous)?;
        }
        CatalogSource::Cloud => {
            let page = state.cloud_target_page.checked_previous()?;
            state.cloud_target_page = page;
            let owner = state
                .cloud_owner
                .clone()
                .ok_or(CatalogControllerError::NoCloudOwner)?;
            request_cloud_refresh(
                state,
                CloudRefreshTarget {
                    owner,
                    revision: state.revision,
                    page,
                    query: state.cloud_query.clone(),
                },
                effects,
            )?;
        }
    }
    Ok(())
}

fn next_page(
    state: &mut CatalogControllerState,
    effects: &mut Vec<CatalogEffect>,
) -> Result<(), CatalogControllerError> {
    if !state.projection.page.has_next {
        return Ok(());
    }
    state.revision = state.revision.checked_next()?;
    match state.source {
        CatalogSource::Local => {
            state.local_page = LocalPageIndex::new(state.local_page.get().saturating_add(1))?;
        }
        CatalogSource::Cloud => {
            let page = state.cloud_target_page.checked_next()?;
            state.cloud_target_page = page;
            let owner = state
                .cloud_owner
                .clone()
                .ok_or(CatalogControllerError::NoCloudOwner)?;
            request_cloud_refresh(
                state,
                CloudRefreshTarget {
                    owner,
                    revision: state.revision,
                    page,
                    query: state.cloud_query.clone(),
                },
                effects,
            )?;
        }
    }
    Ok(())
}

fn prune_local_identity_state(
    state: &mut CatalogControllerState,
    reservation: &dyn ProjectionReservation,
) -> Result<(), CatalogControllerError> {
    let mut posters = reserve_map_clone(
        reservation,
        "controller.posters_prune",
        state.posters.as_ref(),
    )?;
    posters.retain(|path, _| {
        state
            .local_lookup
            .binary_search_by(|candidate| candidate.0.cmp(path))
            .is_ok()
    });
    let mut poster_order = reserve_clone(
        reservation,
        "controller.poster_order_prune",
        state.poster_order.as_slice(),
        0,
    )?;
    poster_order.retain(|path| posters.contains_key(path));
    state.posters = Arc::new(posters);
    state.poster_order = Arc::new(poster_order);

    let mut selected = Vec::new();
    reserve_vec(
        reservation,
        &mut selected,
        "controller.selected_prune",
        state.selected.len(),
    )?;
    selected.extend(
        state
            .selected
            .iter()
            .filter(|identity| {
                identity.local_path().is_some_and(|path| {
                    state
                        .local_lookup
                        .binary_search_by(|candidate| candidate.0.cmp(path))
                        .is_ok()
                })
            })
            .cloned(),
    );
    state.selected = Arc::new(selected);
    if state.active.as_ref().is_some_and(|identity| {
        identity.source() == CatalogSource::Local
            && require_local_identity(state, identity).is_err()
    }) {
        state.active = None;
        state.active_local_target = None;
    }
    if state.menu.as_ref().is_some_and(|menu| {
        menu.target.source() == CatalogSource::Local
            && require_local_identity(state, &menu.target).is_err()
    }) {
        state.menu = None;
    }
    if state.dialog.as_ref().is_some_and(|dialog| {
        dialog.target.source() == CatalogSource::Local
            && require_local_identity(state, &dialog.target).is_err()
    }) {
        state.dialog = None;
        state.pending_detail = None;
        state.pending_mutation = None;
        state.dialog_delete_targets = None;
    }
    Ok(())
}

fn apply_rename(
    state: &mut CatalogControllerState,
    old: ClipPathIdentity,
    result: crate::RenamedClipInfo,
    reservation: &dyn ProjectionReservation,
) -> Result<(), CatalogControllerError> {
    let new = ClipPathIdentity::from_text(&result.path).ok_or(CatalogControllerError::Invalid {
        field: "rename.new_identity",
    })?;
    if new != old
        && state
            .local_lookup
            .binary_search_by(|candidate| candidate.0.cmp(&new))
            .is_ok()
    {
        return Err(CatalogControllerError::Invalid {
            field: "rename.identity_collision",
        });
    }
    let mut items = reserve_clone(
        reservation,
        "controller.local_items_rename",
        state.local_items.as_slice(),
        0,
    )?;
    let lookup_index = state
        .local_lookup
        .binary_search_by(|candidate| candidate.0.cmp(&old))
        .map_err(|_| CatalogControllerError::Invalid {
            field: "rename.missing_identity",
        })?;
    let item_index = state.local_lookup[lookup_index].1;
    let item = &mut items[item_index];
    item.path = result.path;
    item.name = result.name;
    item.title = result.title;
    item.kind = result.kind;
    let mut lookup = reserve_clone(
        reservation,
        "controller.local_lookup_rename",
        state.local_lookup.as_slice(),
        0,
    )?;
    lookup[lookup_index].0 = new.clone();
    lookup.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    state.local_items = Arc::new(items);
    state.local_lookup = Arc::new(lookup);
    migrate_identity(state, &old, &new, reservation)?;
    if state
        .active
        .as_ref()
        .is_some_and(|identity| identity.local_path() == Some(&new))
    {
        let active = CatalogItemIdentity::Local { path: new };
        state.active_local_target = Some(resolve_local(state, &active)?);
    }
    Ok(())
}

fn migrate_identity(
    state: &mut CatalogControllerState,
    old: &ClipPathIdentity,
    new: &ClipPathIdentity,
    reservation: &dyn ProjectionReservation,
) -> Result<(), CatalogControllerError> {
    let old_item = CatalogItemIdentity::Local { path: old.clone() };
    let new_item = CatalogItemIdentity::Local { path: new.clone() };
    if let Ok(index) = state.selected.binary_search(&old_item) {
        let mut selected = reserve_clone(
            reservation,
            "controller.selected_rename",
            state.selected.as_slice(),
            0,
        )?;
        selected[index] = new_item.clone();
        selected.sort_unstable();
        state.selected = Arc::new(selected);
    }
    if state.active.as_ref() == Some(&old_item) {
        state.active = Some(new_item.clone());
    }
    if let Some(menu) = &mut state.menu {
        if menu.target == old_item {
            menu.target = new_item.clone();
        }
    }
    if state
        .dialog
        .as_ref()
        .is_some_and(|dialog| dialog.target == old_item)
    {
        let mut dialog = clone_dialog_for_update(state, reservation)?;
        dialog.target = new_item;
        state.dialog = Some(Arc::new(dialog));
    }
    let mut posters = reserve_map_clone(
        reservation,
        "controller.posters_rename",
        state.posters.as_ref(),
    )?;
    if let Some(status) = posters.remove(old) {
        posters.insert(new.clone(), status);
    }
    state.posters = Arc::new(posters);
    if state.poster_order.iter().any(|path| path == old) {
        let mut order = reserve_clone(
            reservation,
            "controller.poster_order_rename",
            state.poster_order.as_slice(),
            0,
        )?;
        if let Some(path) = order.iter_mut().find(|path| *path == old) {
            *path = new.clone();
        }
        state.poster_order = Arc::new(order);
    }
    Ok(())
}

fn apply_delete_report(
    state: &mut CatalogControllerState,
    targets: &[ClipPathIdentity],
    report: &crate::DeletedClipsReport,
    reservation: &dyn ProjectionReservation,
) -> Result<(), CatalogControllerError> {
    let mut reported = Vec::new();
    reserve_vec(
        reservation,
        &mut reported,
        "controller.delete_report",
        report.deleted.len().saturating_add(report.failed.len()),
    )?;
    for path in report
        .deleted
        .iter()
        .chain(report.failed.iter().map(|(path, _)| path))
    {
        reported.push(ClipPathIdentity::from_text(path).ok_or(
            CatalogControllerError::Invalid {
                field: "delete.report_identity",
            },
        )?);
    }
    reported.sort_unstable();
    if reported.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(CatalogControllerError::Invalid {
            field: "delete.duplicate_report",
        });
    }
    let mut expected = reserve_clone(reservation, "controller.delete_expected", targets, 0)?;
    expected.sort_unstable();
    if reported != expected {
        return Err(CatalogControllerError::Invalid {
            field: "delete.report_targets",
        });
    }
    let mut deleted = Vec::new();
    reserve_vec(
        reservation,
        &mut deleted,
        "controller.deleted",
        report.deleted.len(),
    )?;
    for path in &report.deleted {
        deleted.push(ClipPathIdentity::from_text(path).expect("validated above"));
    }
    deleted.sort_unstable();
    let mut items = Vec::new();
    reserve_vec(
        reservation,
        &mut items,
        "controller.local_items_delete",
        state.local_items.len().saturating_sub(deleted.len()),
    )?;
    items.extend(
        state
            .local_items
            .iter()
            .filter(|item| {
                item.path_identity()
                    .is_some_and(|path| deleted.binary_search(&path).is_err())
            })
            .cloned(),
    );
    let mut lookup = Vec::new();
    reserve_vec(
        reservation,
        &mut lookup,
        "controller.local_lookup_delete",
        items.len(),
    )?;
    for (index, item) in items.iter().enumerate() {
        lookup.push((item.path_identity().expect("accepted item"), index));
    }
    lookup.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    state.local_items = Arc::new(items);
    state.local_lookup = Arc::new(lookup);
    prune_local_identity_state(state, reservation)
}

fn reserve_clone<T: Clone>(
    reservation: &dyn ProjectionReservation,
    field: &'static str,
    source: &[T],
    additional: usize,
) -> Result<Vec<T>, CatalogControllerError> {
    let mut values = Vec::new();
    reserve_vec(
        reservation,
        &mut values,
        field,
        source.len().saturating_add(additional),
    )?;
    values.extend_from_slice(source);
    Ok(values)
}

fn reserve_map_clone<K: Ord + Clone, V: Clone>(
    reservation: &dyn ProjectionReservation,
    field: &'static str,
    source: &BTreeMap<K, V>,
) -> Result<BTreeMap<K, V>, CatalogControllerError> {
    reservation.before_reserve(field, source.len())?;
    Ok(source.clone())
}

fn reserve_vec<T>(
    reservation: &dyn ProjectionReservation,
    values: &mut Vec<T>,
    field: &'static str,
    additional: usize,
) -> Result<(), CatalogControllerError> {
    if additional == 0 {
        return Ok(());
    }
    reservation.before_reserve(field, additional)?;
    values
        .try_reserve_exact(additional)
        .map_err(|_| CatalogControllerError::Allocation { field })
}
