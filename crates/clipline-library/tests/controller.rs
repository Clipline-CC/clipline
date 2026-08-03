//! Integration contract for the bounded catalog reducer.
//!
//! Proposed public API pinned by these compile-first tests:
//! - `CatalogController::new(Arc<dyn LocalDayResolver>)` builds a detached,
//!   empty Local controller using the system reservation policy.
//! - `CatalogController::with_reservation(days, reservation)` injects the
//!   existing `ProjectionReservation` seam for controller staging as well as
//!   projection staging.
//! - `attach`, `detach`, `set_cloud_owner`, `dispatch`, and `accept` are the
//!   only reducer inputs. Each returns a bounded `Vec<CatalogEffect>`.
//! - `state()` returns a complete, shallow-cloneable `CatalogControllerState`.
//!   Its `PartialEq` includes pending/dirty refresh ownership, so rejecting a
//!   stale or malformed result can be pinned as a byte-for-byte-equivalent
//!   reducer checkpoint without exposing service or UI handles.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use clipline_library::{
    CatalogAction, CatalogCloudPreferences, CatalogController, CatalogDialogKind, CatalogEffect,
    CatalogItemIdentity, CatalogLoadState, CatalogOperationOwner, CatalogResult, CatalogRevision,
    CatalogSource, CatalogUploadVisibility, ClipDetail, ClipDetailRequest, ClipDetailResult,
    ClipPathIdentity, CloudAccountGeneration, CloudAccountKey, CloudCatalogOwner, CloudLibraryItem,
    CloudListPageCompletion, CloudMediaLeaseId, CloudPageNumber, CloudReviewMediaOwner,
    CloudThumbnailDescriptor, CloudThumbnailOwner, CloudWorkToken, DeletedClipsReport,
    DurableUploadToken, ForegroundGeneration, LocalClipGrouping, LocalClipId, LocalClipItem,
    LocalDay, LocalDayResolver, LocalIndexCompletion, LocalPageIndex, PosterGeneration,
    PosterResult, PosterStatus, PosterWorkToken, PreparedCloudReviewMedia, PresentationError,
    PresentationPoster, ProjectionReservation, RemoteClipId, RenamedClipInfo, RequestGeneration,
    UploadDialogSummary, UploadGeneration, UploadSummary, WindowAttachmentGeneration,
    WindowWorkToken, MAX_CATALOG_EFFECTS_PER_UPDATE, MAX_CLOUD_THUMBNAIL_REQUESTS_PER_UPDATE,
    MAX_LOCAL_INDEX_ROWS, MAX_POSTER_RESULT_ENTRIES, MAX_UPLOAD_SUMMARIES,
};

#[derive(Clone, Copy)]
struct TestDays;

impl LocalDayResolver for TestDays {
    fn today_start_unix(&self) -> u64 {
        2_000_000
    }

    fn resolve_day(&self, timestamp: u64) -> LocalDay {
        LocalDay {
            key: format!("day-{}", timestamp / 86_400),
            label: "Test day".into(),
        }
    }
}

#[derive(Default)]
struct ArmedReservation {
    armed: AtomicBool,
}

impl ArmedReservation {
    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct ArmedCloudProjectionReservation {
    armed: AtomicBool,
}

impl ArmedCloudProjectionReservation {
    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }
}

impl ProjectionReservation for ArmedCloudProjectionReservation {
    fn before_reserve(
        &self,
        field: &'static str,
        _additional: usize,
    ) -> Result<(), PresentationError> {
        if field == "projection.rows" && self.armed.swap(false, Ordering::SeqCst) {
            Err(PresentationError::Allocation { field })
        } else {
            Ok(())
        }
    }
}

impl ProjectionReservation for ArmedReservation {
    fn before_reserve(
        &self,
        field: &'static str,
        _additional: usize,
    ) -> Result<(), PresentationError> {
        if field == "controller.local_index" && self.armed.swap(false, Ordering::SeqCst) {
            Err(PresentationError::Allocation { field })
        } else {
            Ok(())
        }
    }
}

fn controller() -> CatalogController {
    CatalogController::new(Arc::new(TestDays)).unwrap()
}

fn local_item(index: usize) -> LocalClipItem {
    LocalClipItem {
        path: format!(r"C:\Clips\Clip-{index:05}.mp4"),
        name: format!("Clip-{index:05}.mp4"),
        title: Some(format!("Clip {index:05}")),
        kind: "replay".into(),
        session: Some(format!("Session-{}", index / 10)),
        size_mb: 1.0,
        modified_unix: index as u64,
        duration_s: Some(7.0),
        marker_count: 0,
        game: None,
        file_identity: None,
        marker_summary: Default::default(),
    }
}

fn local_identity(index: usize) -> CatalogItemIdentity {
    CatalogItemIdentity::Local {
        path: ClipPathIdentity::from_text(&local_item(index).path).unwrap(),
    }
}

#[test]
fn local_mutation_effect_carries_the_scanned_file_identity() {
    let directory = clipline_test_utils::TestDir::new("catalog-controller", "file-identity");
    let path = directory.path().join("one.mp4");
    std::fs::write(&path, b"one").unwrap();
    let opened = clipline_shell::open_regular_file_nofollow(&path).unwrap();
    let file_identity = clipline_shell::opened_file_identity(&opened).unwrap();
    drop(opened);

    let mut item = local_item(0);
    item.path = path.display().to_string();
    item.name = "one.mp4".into();
    item.file_identity = Some(file_identity);
    let identity = CatalogItemIdentity::Local {
        path: item.path_identity().unwrap(),
    };
    let mut controller = controller();
    seed_local(&mut controller, vec![item]);
    controller
        .dispatch(CatalogAction::OpenRenameTitle { item: identity })
        .unwrap();
    let effect = only_effect(controller.dispatch(CatalogAction::ConfirmDialog).unwrap());
    match effect {
        CatalogEffect::RenameTitle { target, .. } => {
            assert_eq!(target.expected_file_identity, Some(file_identity));
        }
        other => panic!("expected rename-title effect, got {other:?}"),
    }
}

fn cloud_owner(account: &str, generation: u64) -> CloudCatalogOwner {
    CloudCatalogOwner {
        account_key: CloudAccountKey::new(account).unwrap(),
        account_generation: CloudAccountGeneration::new(generation),
    }
}

fn cloud_item(id: &str) -> CloudLibraryItem {
    CloudLibraryItem {
        remote_clip_id: id.into(),
        local_clip_id: None,
        path: String::new(),
        title: format!("Cloud {id}"),
        remote_url: format!("https://clips.example/c/{id}"),
        visibility: "public".into(),
        upload_status: "uploaded_public".into(),
        updated_at_unix: 1,
        uploaded_at_unix: None,
        duration_ms: Some(1_000),
        file_size_bytes: Some(1_024),
        source_type: Some("replay".into()),
    }
}

fn upload_token(index: usize, generation: u64) -> DurableUploadToken {
    DurableUploadToken {
        account_key: CloudAccountKey::new("account-a").unwrap(),
        account_generation: CloudAccountGeneration::new(9),
        upload_generation: UploadGeneration::new(generation),
        local_clip_id: LocalClipId::new(format!("local-{index}")).unwrap(),
        source_path: ClipPathIdentity::from_text(&local_item(index).path).unwrap(),
    }
}

fn upload_summary(index: usize, status: &str) -> UploadSummary {
    UploadSummary {
        local_clip_id: format!("local-{index}"),
        path: local_item(index).path,
        upload_status: status.to_owned(),
        received_size_bytes: 512,
        file_size_bytes: 1_024,
        remote_clip_id: None,
        remote_url: None,
        error: None,
    }
}

fn only_local_refresh(effects: Vec<CatalogEffect>) -> (WindowWorkToken, CatalogRevision) {
    match effects.as_slice() {
        [CatalogEffect::RefreshLocal { token, revision }] => (*token, *revision),
        other => panic!("expected one local refresh, got {other:?}"),
    }
}

fn only_cloud_refresh(
    effects: Vec<CatalogEffect>,
) -> (CloudWorkToken, CatalogRevision, CloudPageNumber) {
    match effects.as_slice() {
        [CatalogEffect::RefreshCloud {
            token,
            revision,
            page,
            ..
        }] => (token.clone(), *revision, *page),
        other => panic!("expected one cloud refresh, got {other:?}"),
    }
}

fn attach(
    controller: &mut CatalogController,
    attachment: u64,
) -> (WindowWorkToken, CatalogRevision) {
    only_local_refresh(
        controller
            .attach(
                WindowAttachmentGeneration::new(attachment),
                ForegroundGeneration::new(attachment + 100),
            )
            .unwrap(),
    )
}

fn accept_local(
    controller: &mut CatalogController,
    token: WindowWorkToken,
    revision: CatalogRevision,
    truncated: bool,
    items: Vec<LocalClipItem>,
) -> Vec<CatalogEffect> {
    controller
        .accept(CatalogResult::LocalIndex(
            LocalIndexCompletion::new(token, revision, truncated, items, Vec::new()).unwrap(),
        ))
        .unwrap()
}

fn seed_local(controller: &mut CatalogController, items: Vec<LocalClipItem>) {
    let (token, revision) = attach(controller, 1);
    assert!(accept_local(controller, token, revision, false, items).is_empty());
}

fn seed_cloud(controller: &mut CatalogController, account: &str) -> CatalogItemIdentity {
    seed_local(controller, vec![local_item(0)]);
    let owner = cloud_owner(account, 1);
    controller.set_cloud_owner(Some(owner.clone())).unwrap();
    let (token, revision, page) = only_cloud_refresh(
        controller
            .dispatch(CatalogAction::SetSource {
                source: CatalogSource::Cloud,
            })
            .unwrap(),
    );
    controller
        .accept(CatalogResult::CloudPage(
            CloudListPageCompletion::page(
                token,
                revision,
                page,
                vec![cloud_item("remote-a")],
                Vec::new(),
            )
            .unwrap(),
        ))
        .unwrap();
    CatalogItemIdentity::Cloud {
        account_key: owner.account_key,
        account_generation: owner.account_generation,
        remote_clip_id: RemoteClipId::new("remote-a").unwrap(),
    }
}

fn operation_owner(effect: &CatalogEffect) -> CatalogOperationOwner {
    effect.operation_owner().unwrap().unwrap()
}

fn only_effect(effects: Vec<CatalogEffect>) -> CatalogEffect {
    let [effect]: [CatalogEffect; 1] = effects.try_into().unwrap();
    effect
}

fn stale_window(token: WindowWorkToken) -> WindowWorkToken {
    WindowWorkToken {
        request: token.request.checked_next().unwrap(),
        ..token
    }
}

#[test]
fn stale_window_revision_and_cloud_account_results_leave_state_identical() {
    let mut controller = controller();
    seed_local(&mut controller, vec![local_item(0)]);

    let (token, revision) =
        only_local_refresh(controller.dispatch(CatalogAction::Refresh).unwrap());
    let before = controller.state().clone();

    let stale_window = WindowWorkToken {
        attachment: WindowAttachmentGeneration::new(token.attachment.get() + 1),
        ..token
    };
    assert!(accept_local(
        &mut controller,
        stale_window,
        revision,
        false,
        vec![local_item(1)],
    )
    .is_empty());
    assert_eq!(controller.state(), &before);

    assert!(accept_local(
        &mut controller,
        token,
        revision.checked_next().unwrap(),
        false,
        vec![local_item(1)],
    )
    .is_empty());
    assert_eq!(controller.state(), &before);

    assert!(accept_local(&mut controller, token, revision, false, vec![local_item(0)],).is_empty());

    let owner = cloud_owner("account-a", 7);
    assert!(controller
        .set_cloud_owner(Some(owner.clone()))
        .unwrap()
        .is_empty());
    let (cloud_token, cloud_revision, page) = only_cloud_refresh(
        controller
            .dispatch(CatalogAction::SetSource {
                source: CatalogSource::Cloud,
            })
            .unwrap(),
    );
    let cloud_before = controller.state().clone();
    let wrong_account = CloudWorkToken {
        account_key: CloudAccountKey::new("account-b").unwrap(),
        ..cloud_token.clone()
    };
    let stale = CloudListPageCompletion::page(
        wrong_account,
        cloud_revision,
        page,
        vec![cloud_item("remote-1")],
        Vec::new(),
    )
    .unwrap();
    assert!(controller
        .accept(CatalogResult::CloudPage(stale))
        .unwrap()
        .is_empty());
    assert_eq!(controller.state(), &cloud_before);
}

#[test]
fn local_refresh_is_one_in_flight_plus_one_latest_dirty_target() {
    let mut controller = controller();
    let (first_token, first_revision) = attach(&mut controller, 1);

    assert!(controller
        .dispatch(CatalogAction::Refresh)
        .unwrap()
        .is_empty());
    assert!(controller
        .dispatch(CatalogAction::Refresh)
        .unwrap()
        .is_empty());

    let (latest_token, latest_revision) = only_local_refresh(accept_local(
        &mut controller,
        first_token,
        first_revision,
        false,
        vec![local_item(0)],
    ));
    assert_eq!(latest_token.attachment, first_token.attachment);
    assert_eq!(
        latest_revision,
        first_revision
            .checked_next()
            .unwrap()
            .checked_next()
            .unwrap()
    );

    let before_stale = controller.state().clone();
    assert!(accept_local(
        &mut controller,
        first_token,
        first_revision,
        false,
        vec![local_item(99)],
    )
    .is_empty());
    assert_eq!(controller.state(), &before_stale);

    assert!(accept_local(
        &mut controller,
        latest_token,
        latest_revision,
        false,
        vec![local_item(2)],
    )
    .is_empty());
    assert_eq!(controller.state().local_items.as_slice(), &[local_item(2)]);
}

#[test]
fn accepted_refresh_publishes_a_new_revision_without_rewinding_intervening_actions() {
    let mut controller = controller();
    let (token, requested_revision) = attach(&mut controller, 1);
    controller.dispatch(CatalogAction::EnterSelection).unwrap();
    let intervening_revision = controller.state().revision;
    assert!(intervening_revision > requested_revision);

    assert!(accept_local(
        &mut controller,
        token,
        requested_revision,
        false,
        vec![local_item(0)],
    )
    .is_empty());
    assert!(controller.state().revision > intervening_revision);
    assert_eq!(
        controller.state().projection.revision,
        controller.state().revision
    );
}

#[test]
fn detach_preserves_state_and_reattach_reissues_pending_work_with_a_new_fence() {
    let mut controller = controller();
    let (old_token, revision) = attach(&mut controller, 1);
    let accepted_before_detach = controller.state().local_items.clone();

    assert!(controller.detach().unwrap().is_empty());
    assert_eq!(controller.state().local_items, accepted_before_detach);
    let detached = controller.state().clone();

    let (new_token, reissued_revision) = attach(&mut controller, 2);
    assert_ne!(new_token.attachment, old_token.attachment);
    assert_ne!(new_token.foreground, old_token.foreground);
    assert_ne!(new_token.request, old_token.request);
    assert_eq!(reissued_revision, revision);

    let attached = controller.state().clone();
    assert_ne!(attached, detached);
    assert!(accept_local(
        &mut controller,
        old_token,
        revision,
        false,
        vec![local_item(99)],
    )
    .is_empty());
    assert_eq!(controller.state(), &attached);

    assert!(accept_local(
        &mut controller,
        new_token,
        reissued_revision,
        false,
        vec![local_item(0)],
    )
    .is_empty());
    assert_eq!(controller.state().projection.rows.len(), 1);
}

#[test]
fn reattach_reprojects_complete_data_without_an_unneeded_refresh_but_reconciles_a_mutation() {
    let mut controller = controller();
    seed_local(&mut controller, vec![local_item(0)]);
    let accepted_projection = controller.state().projection.clone();
    assert!(controller.detach().unwrap().is_empty());
    assert!(controller
        .attach(
            WindowAttachmentGeneration::new(2),
            ForegroundGeneration::new(102),
        )
        .unwrap()
        .is_empty());
    assert_eq!(controller.state().projection, accepted_projection);

    let item = local_identity(0);
    controller
        .dispatch(CatalogAction::OpenRenameTitle { item })
        .unwrap();
    assert!(matches!(
        controller
            .dispatch(CatalogAction::ConfirmDialog)
            .unwrap()
            .as_slice(),
        [CatalogEffect::RenameTitle { .. }]
    ));
    assert!(controller.detach().unwrap().is_empty());
    assert!(matches!(
        controller
            .attach(
                WindowAttachmentGeneration::new(3),
                ForegroundGeneration::new(103),
            )
            .unwrap()
            .as_slice(),
        [CatalogEffect::RefreshLocal { .. }]
    ));
}

#[test]
fn detach_closes_window_playback_but_reattach_reopens_the_retained_active_item() {
    let mut controller = controller();
    seed_local(&mut controller, vec![local_item(0)]);
    let item = local_identity(0);
    assert!(matches!(
        controller
            .dispatch(CatalogAction::OpenItem { item: item.clone() })
            .unwrap()
            .as_slice(),
        [CatalogEffect::OpenLocalReview { .. }]
    ));
    assert!(matches!(
        controller.detach().unwrap().as_slice(),
        [CatalogEffect::CloseReview { .. }]
    ));
    assert_eq!(controller.state().active.as_ref(), Some(&item));
    assert!(matches!(
        controller
            .attach(
                WindowAttachmentGeneration::new(2),
                ForegroundGeneration::new(102),
            )
            .unwrap()
            .as_slice(),
        [CatalogEffect::OpenLocalReview { .. }]
    ));
    assert_eq!(controller.state().active.as_ref(), Some(&item));
}

#[test]
fn truncated_replacement_preserves_omitted_targets_but_complete_replacement_prunes_them() {
    let mut controller = controller();
    seed_local(
        &mut controller,
        vec![local_item(0), local_item(1), local_item(2)],
    );
    let omitted = local_identity(1);

    controller
        .dispatch(CatalogAction::OpenItem {
            item: omitted.clone(),
        })
        .unwrap();
    controller.dispatch(CatalogAction::EnterSelection).unwrap();
    controller
        .dispatch(CatalogAction::ToggleSelection {
            item: omitted.clone(),
        })
        .unwrap();
    controller
        .dispatch(CatalogAction::OpenDelete {
            item: omitted.clone(),
        })
        .unwrap();

    let (token, revision) =
        only_local_refresh(controller.dispatch(CatalogAction::Refresh).unwrap());
    assert!(accept_local(&mut controller, token, revision, true, vec![local_item(0)],).is_empty());
    assert!(controller.state().local_truncated);
    assert_eq!(
        controller.state().selected.as_slice(),
        std::slice::from_ref(&omitted)
    );
    assert_eq!(controller.state().active.as_ref(), Some(&omitted));
    assert_eq!(
        controller
            .state()
            .dialog
            .as_ref()
            .map(|dialog| &dialog.target),
        Some(&omitted)
    );

    // Restore the full authority, replace the modal with a context target,
    // then prove that target gets the same truncated-scan treatment.
    let (token, revision) =
        only_local_refresh(controller.dispatch(CatalogAction::Refresh).unwrap());
    assert!(accept_local(
        &mut controller,
        token,
        revision,
        false,
        vec![local_item(0), local_item(1)],
    )
    .is_empty());
    controller.dispatch(CatalogAction::CancelDialog).unwrap();
    controller
        .dispatch(CatalogAction::OpenContext {
            item: omitted.clone(),
        })
        .unwrap();
    let (token, revision) =
        only_local_refresh(controller.dispatch(CatalogAction::Refresh).unwrap());
    assert!(accept_local(&mut controller, token, revision, true, vec![local_item(0)],).is_empty());
    assert_eq!(
        controller.state().menu.as_ref().map(|menu| &menu.target),
        Some(&omitted)
    );

    let (token, revision) =
        only_local_refresh(controller.dispatch(CatalogAction::Refresh).unwrap());
    assert!(accept_local(&mut controller, token, revision, false, vec![local_item(0)],).is_empty());
    assert!(!controller.state().local_truncated);
    assert!(controller.state().selected.is_empty());
    assert!(controller.state().active.is_none());
    assert!(controller.state().menu.is_none());
    assert!(controller.state().dialog.is_none());
}

#[test]
fn selection_is_sorted_survives_page_changes_and_is_cleared_by_cloud_switch() {
    let mut controller = controller();
    seed_local(&mut controller, (0..65).map(local_item).collect::<Vec<_>>());
    controller.dispatch(CatalogAction::EnterSelection).unwrap();
    for index in [64, 30, 0] {
        controller
            .dispatch(CatalogAction::ToggleSelection {
                item: local_identity(index),
            })
            .unwrap();
    }
    let expected = Arc::new(vec![
        local_identity(0),
        local_identity(30),
        local_identity(64),
    ]);
    assert_eq!(controller.state().selected, expected);

    controller
        .dispatch(CatalogAction::SetLocalPage {
            page: LocalPageIndex::new(1).unwrap(),
        })
        .unwrap();
    controller
        .dispatch(CatalogAction::SetLocalGrouping {
            grouping: LocalClipGrouping::Session,
        })
        .unwrap();
    assert_eq!(controller.state().selected, expected);
    assert_eq!(controller.state().projection.selected_count, 3);

    controller
        .set_cloud_owner(Some(cloud_owner("account-a", 1)))
        .unwrap();
    let effects = controller
        .dispatch(CatalogAction::SetSource {
            source: CatalogSource::Cloud,
        })
        .unwrap();
    only_cloud_refresh(effects);
    assert!(controller.state().selected.is_empty());
    assert!(!controller.state().selection_mode);
}

#[test]
fn cloud_source_without_an_account_projects_the_disconnected_state() {
    let mut controller = controller();
    seed_local(&mut controller, vec![local_item(0)]);
    controller.dispatch(CatalogAction::EnterSelection).unwrap();
    controller
        .dispatch(CatalogAction::ToggleSelection {
            item: local_identity(0),
        })
        .unwrap();

    assert!(controller
        .dispatch(CatalogAction::SetSource {
            source: CatalogSource::Cloud,
        })
        .unwrap()
        .is_empty());
    assert_eq!(controller.state().source, CatalogSource::Cloud);
    assert_eq!(
        controller.state().projection.load_state,
        CatalogLoadState::Disconnected
    );
    assert!(controller.state().projection.rows.is_empty());
    assert_eq!(controller.state().projection.page.range_text, "0");
    assert!(controller.state().selected.is_empty());
    assert!(!controller.state().selection_mode);

    assert!(controller
        .dispatch(CatalogAction::SetQuery {
            query: "favorite".into(),
        })
        .unwrap()
        .is_empty());
    assert!(controller
        .dispatch(CatalogAction::Refresh)
        .unwrap()
        .is_empty());

    let owner = cloud_owner("account-a", 1);
    let effects = controller.set_cloud_owner(Some(owner)).unwrap();
    only_cloud_refresh(effects);
}

#[test]
fn escape_consumes_exactly_one_layer_in_the_documented_priority_order() {
    let mut controller = controller();
    seed_local(&mut controller, vec![local_item(0)]);
    let item = local_identity(0);
    controller
        .dispatch(CatalogAction::OpenItem { item: item.clone() })
        .unwrap();
    controller.dispatch(CatalogAction::EnterSelection).unwrap();
    controller
        .dispatch(CatalogAction::ToggleSelection { item: item.clone() })
        .unwrap();
    controller
        .dispatch(CatalogAction::OpenDelete { item: item.clone() })
        .unwrap();

    assert!(controller.state().dialog.is_some());
    assert!(controller.state().active.is_some());

    assert!(controller
        .dispatch(CatalogAction::Escape)
        .unwrap()
        .is_empty());
    assert!(controller.state().dialog.is_none());
    controller
        .dispatch(CatalogAction::OpenContext { item })
        .unwrap();
    assert!(controller.state().menu.is_some());

    assert!(controller
        .dispatch(CatalogAction::Escape)
        .unwrap()
        .is_empty());
    assert!(controller.state().menu.is_none());
    assert_eq!(controller.state().selected.len(), 1);

    assert!(controller
        .dispatch(CatalogAction::Escape)
        .unwrap()
        .is_empty());
    assert!(controller.state().selected.is_empty());
    assert!(controller.state().selection_mode);
    assert!(controller.state().active.is_some());

    assert!(controller
        .dispatch(CatalogAction::Escape)
        .unwrap()
        .is_empty());
    assert!(!controller.state().selection_mode);
    assert!(controller.state().active.is_some());

    let effects = controller.dispatch(CatalogAction::Escape).unwrap();
    assert!(matches!(
        effects.as_slice(),
        [CatalogEffect::CloseReview { .. }]
    ));
    assert!(controller.state().active.is_none());
}

#[test]
fn malformed_duplicate_and_failed_ten_thousand_row_staging_roll_back_atomically() {
    let reservation = Arc::new(ArmedReservation::default());
    let mut controller =
        CatalogController::with_reservation(Arc::new(TestDays), reservation.clone()).unwrap();
    seed_local(&mut controller, vec![local_item(0)]);

    let (token, revision) =
        only_local_refresh(controller.dispatch(CatalogAction::Refresh).unwrap());
    let accepted_items = controller.state().local_items.clone();
    let duplicate = LocalIndexCompletion::new(
        token,
        revision,
        false,
        vec![local_item(1), local_item(1)],
        Vec::new(),
    )
    .unwrap();
    assert!(controller
        .accept(CatalogResult::LocalIndex(duplicate))
        .unwrap()
        .is_empty());
    assert_eq!(controller.state().local_items, accepted_items);
    assert!(matches!(
        controller.state().projection.load_state,
        CatalogLoadState::Error { .. }
    ));

    let (token, revision) =
        only_local_refresh(controller.dispatch(CatalogAction::Refresh).unwrap());

    let mut malformed =
        LocalIndexCompletion::new(token, revision, false, vec![local_item(1)], Vec::new()).unwrap();
    malformed.items[0].path.clear();
    assert!(controller
        .accept(CatalogResult::LocalIndex(malformed))
        .unwrap()
        .is_empty());
    assert_eq!(controller.state().local_items, accepted_items);

    let (token, revision) =
        only_local_refresh(controller.dispatch(CatalogAction::Refresh).unwrap());

    reservation.arm();
    let maximum = (0..MAX_LOCAL_INDEX_ROWS)
        .map(local_item)
        .collect::<Vec<_>>();
    let maximum = LocalIndexCompletion::new(token, revision, false, maximum, Vec::new()).unwrap();
    assert!(controller
        .accept(CatalogResult::LocalIndex(maximum))
        .unwrap()
        .is_empty());
    assert_eq!(controller.state().local_items, accepted_items);
    only_local_refresh(controller.dispatch(CatalogAction::Refresh).unwrap());
}

#[test]
fn poster_retention_is_fifo_bounded_and_complete_scans_prune_removed_paths() {
    let mut controller = controller();
    let (window, revision) = attach(&mut controller, 1);
    assert!(accept_local(
        &mut controller,
        window,
        revision,
        false,
        (0..=MAX_POSTER_RESULT_ENTRIES).map(local_item).collect(),
    )
    .is_empty());

    for index in 0..=MAX_POSTER_RESULT_ENTRIES {
        let path = ClipPathIdentity::from_text(&local_item(index).path).unwrap();
        assert!(controller
            .accept(CatalogResult::Poster {
                token: PosterWorkToken {
                    window,
                    poster: PosterGeneration::new(index as u64 + 1),
                    path: path.clone(),
                },
                poster: PosterResult {
                    path,
                    status: PosterStatus::Ready {
                        path: format!(r"C:\Cache\Poster-{index:05}.jpg"),
                    },
                },
            })
            .unwrap()
            .is_empty());
    }
    controller
        .dispatch(CatalogAction::SetLocalPage {
            page: LocalPageIndex::new(2).unwrap(),
        })
        .unwrap();
    let oldest = controller
        .state()
        .projection
        .rows
        .iter()
        .find(|row| row.identity == local_identity(0))
        .unwrap();
    assert_eq!(
        oldest.poster,
        PresentationPoster::Missing,
        "the oldest poster entry is evicted at the hard cap"
    );

    let (token, revision) =
        only_local_refresh(controller.dispatch(CatalogAction::Refresh).unwrap());
    assert!(accept_local(
        &mut controller,
        token,
        revision,
        false,
        vec![local_item(MAX_POSTER_RESULT_ENTRIES)],
    )
    .is_empty());
    assert_eq!(
        controller.state().projection.rows[0].poster,
        PresentationPoster::Ready {
            path: format!(r"C:\Cache\Poster-{:05}.jpg", MAX_POSTER_RESULT_ENTRIES)
        }
    );
}

#[test]
fn cloud_completion_accepts_only_the_exact_current_owner_revision_and_request() {
    let mut controller = controller();
    seed_local(&mut controller, vec![local_item(0)]);
    let owner = cloud_owner("account-a", 3);
    controller.set_cloud_owner(Some(owner.clone())).unwrap();
    let (token, revision, page) = only_cloud_refresh(
        controller
            .dispatch(CatalogAction::SetSource {
                source: CatalogSource::Cloud,
            })
            .unwrap(),
    );

    let exact = CloudListPageCompletion::page(
        token,
        revision,
        page,
        vec![cloud_item("remote-a")],
        Vec::new(),
    )
    .unwrap();
    assert!(matches!(
        controller
            .accept(CatalogResult::CloudPage(exact))
            .unwrap()
            .as_slice(),
        [CatalogEffect::LoadCloudThumbnail { .. }]
    ));
    assert_eq!(controller.state().projection.rows.len(), 1);
    assert_eq!(
        controller.state().projection.rows[0].identity,
        CatalogItemIdentity::Cloud {
            account_key: owner.account_key,
            account_generation: owner.account_generation,
            remote_clip_id: RemoteClipId::new("remote-a").unwrap(),
        }
    );
}

#[test]
fn accepted_cloud_page_issues_versioned_bounded_thumbnail_work_and_rejects_stale_results() {
    let mut controller = controller();
    seed_local(&mut controller, vec![local_item(0)]);
    let owner = cloud_owner("account-a", 7);
    controller.set_cloud_owner(Some(owner)).unwrap();
    let (token, revision, page) = only_cloud_refresh(
        controller
            .dispatch(CatalogAction::SetSource {
                source: CatalogSource::Cloud,
            })
            .unwrap(),
    );
    let items = (0..MAX_CLOUD_THUMBNAIL_REQUESTS_PER_UPDATE)
        .map(|index| {
            let mut item = cloud_item(&format!("remote-{index}"));
            item.updated_at_unix = 10_000 + index as u64;
            item
        })
        .collect();
    let effects = controller
        .accept(CatalogResult::CloudPage(
            CloudListPageCompletion::page(token.clone(), revision, page, items, Vec::new())
                .unwrap(),
        ))
        .unwrap();
    assert_eq!(effects.len(), MAX_CLOUD_THUMBNAIL_REQUESTS_PER_UPDATE);
    assert!(effects.len() <= MAX_CATALOG_EFFECTS_PER_UPDATE);
    let first = match &effects[0] {
        CatalogEffect::LoadCloudThumbnail { request } => {
            assert_eq!(request.owner.token, token);
            assert_eq!(request.owner.descriptor.version, 10_000);
            request.owner.clone()
        }
        other => panic!("expected thumbnail effect, got {other:?}"),
    };
    assert!(controller
        .state()
        .projection
        .rows
        .iter()
        .all(|row| row.poster == PresentationPoster::Queued));

    controller
        .accept(CatalogResult::CloudThumbnail {
            owner: first.clone(),
            status: PosterStatus::Queued,
        })
        .unwrap();
    controller
        .accept(CatalogResult::CloudThumbnail {
            owner: first.clone(),
            status: PosterStatus::Ready {
                path: r"C:\Cache\cloud-thumb.jpg".into(),
            },
        })
        .unwrap();
    assert_eq!(
        controller.state().projection.rows[0].poster,
        PresentationPoster::Ready {
            path: r"C:\Cache\cloud-thumb.jpg".into()
        }
    );

    let checkpoint = controller.state().clone();
    let stale_version = CloudThumbnailOwner::new(
        first.token.clone(),
        CloudThumbnailDescriptor::new(first.descriptor.item.clone(), 9_999).unwrap(),
    )
    .unwrap();
    assert!(controller
        .accept(CatalogResult::CloudThumbnail {
            owner: stale_version,
            status: PosterStatus::Missing,
        })
        .unwrap()
        .is_empty());
    assert_eq!(controller.state(), &checkpoint);

    let mut stale_window_token = first.token.clone();
    stale_window_token.window.request = RequestGeneration::new(999);
    let stale_window =
        CloudThumbnailOwner::new(stale_window_token, first.descriptor.clone()).unwrap();
    assert!(controller
        .accept(CatalogResult::CloudThumbnail {
            owner: stale_window,
            status: PosterStatus::Missing,
        })
        .unwrap()
        .is_empty());
    assert_eq!(controller.state(), &checkpoint);

    let stale_account_item = CatalogItemIdentity::Cloud {
        account_key: CloudAccountKey::new("account-b").unwrap(),
        account_generation: CloudAccountGeneration::new(8),
        remote_clip_id: RemoteClipId::new("remote-0").unwrap(),
    };
    let mut stale_account_token = first.token;
    stale_account_token.account_key = CloudAccountKey::new("account-b").unwrap();
    stale_account_token.account_generation = CloudAccountGeneration::new(8);
    let stale_account = CloudThumbnailOwner::new(
        stale_account_token,
        CloudThumbnailDescriptor::new(stale_account_item, 10_000).unwrap(),
    )
    .unwrap();
    assert!(controller
        .accept(CatalogResult::CloudThumbnail {
            owner: stale_account,
            status: PosterStatus::Missing,
        })
        .unwrap()
        .is_empty());
    assert_eq!(controller.state(), &checkpoint);
}

#[test]
fn exact_refresh_failures_clear_only_the_owned_lane_and_launch_only_the_latest_dirty_target() {
    let mut controller = controller();
    let first = only_effect(
        controller
            .attach(
                WindowAttachmentGeneration::new(1),
                ForegroundGeneration::new(2),
            )
            .unwrap(),
    );
    let first_owner = operation_owner(&first);
    assert!(controller
        .dispatch(CatalogAction::Refresh)
        .unwrap()
        .is_empty());
    assert!(controller
        .dispatch(CatalogAction::Refresh)
        .unwrap()
        .is_empty());

    let latest = only_effect(
        controller
            .accept(CatalogResult::OperationFailed {
                owner: first_owner.clone(),
                message: "first refresh failed".into(),
            })
            .unwrap(),
    );
    let (first_revision, latest_revision) = match (&first_owner, &latest) {
        (
            CatalogOperationOwner::LocalRefresh {
                revision: first, ..
            },
            CatalogEffect::RefreshLocal {
                revision: latest, ..
            },
        ) => (*first, *latest),
        other => panic!("expected local refresh owners, got {other:?}"),
    };
    assert_eq!(
        latest_revision,
        first_revision
            .checked_next()
            .unwrap()
            .checked_next()
            .unwrap()
    );

    let before_stale = controller.state().clone();
    assert!(controller
        .accept(CatalogResult::OperationFailed {
            owner: first_owner,
            message: "late duplicate".into(),
        })
        .unwrap()
        .is_empty());
    assert_eq!(controller.state(), &before_stale);

    assert!(controller
        .accept(CatalogResult::OperationFailed {
            owner: operation_owner(&latest),
            message: "latest refresh failed".into(),
        })
        .unwrap()
        .is_empty());
    assert!(matches!(
        &controller.state().projection.load_state,
        clipline_library::CatalogLoadState::Error { message }
            if message == "latest refresh failed"
    ));

    let cloud_item = seed_cloud(&mut controller, "account-a");
    assert_eq!(controller.state().projection.rows[0].identity, cloud_item);
    let current = only_effect(controller.dispatch(CatalogAction::Refresh).unwrap());
    let exact_owner = operation_owner(&current);
    let stale_owner = match &exact_owner {
        CatalogOperationOwner::CloudRefresh {
            token,
            revision,
            page,
        } => CatalogOperationOwner::CloudRefresh {
            token: CloudWorkToken {
                account_key: CloudAccountKey::new("replacement-account").unwrap(),
                ..token.clone()
            },
            revision: *revision,
            page: *page,
        },
        other => panic!("expected cloud refresh, got {other:?}"),
    };
    let before_stale = controller.state().clone();
    assert!(controller
        .accept(CatalogResult::OperationFailed {
            owner: stale_owner,
            message: "stale account".into(),
        })
        .unwrap()
        .is_empty());
    assert_eq!(controller.state(), &before_stale);
    assert!(controller
        .accept(CatalogResult::OperationFailed {
            owner: exact_owner,
            message: "cloud refresh failed".into(),
        })
        .unwrap()
        .is_empty());
    assert!(matches!(
        &controller.state().projection.load_state,
        clipline_library::CatalogLoadState::Error { message }
            if message == "cloud refresh failed"
    ));
}

#[test]
fn exact_detail_rename_and_delete_failures_clear_only_the_matching_pending_operation() {
    let mut controller = controller();
    seed_local(&mut controller, vec![local_item(0), local_item(1)]);
    let item = local_identity(0);

    let detail = only_effect(
        controller
            .dispatch(CatalogAction::OpenUpload { item: item.clone() })
            .unwrap(),
    );
    let detail_owner = operation_owner(&detail);
    let stale_detail = match &detail_owner {
        CatalogOperationOwner::ClipDetail { owner } => CatalogOperationOwner::ClipDetail {
            owner: ClipDetailRequest::new(owner.item().clone(), stale_window(owner.window()))
                .owner()
                .clone(),
        },
        other => panic!("expected detail owner, got {other:?}"),
    };
    let before_stale = controller.state().clone();
    controller
        .accept(CatalogResult::OperationFailed {
            owner: stale_detail,
            message: "stale detail".into(),
        })
        .unwrap();
    assert_eq!(controller.state(), &before_stale);
    controller
        .accept(CatalogResult::OperationFailed {
            owner: detail_owner.clone(),
            message: "detail failed".into(),
        })
        .unwrap();
    assert_ne!(controller.state(), &before_stale);
    let after_detail = controller.state().clone();
    controller
        .accept(CatalogResult::OperationFailed {
            owner: detail_owner,
            message: "late detail".into(),
        })
        .unwrap();
    assert_eq!(controller.state(), &after_detail);

    controller
        .dispatch(CatalogAction::OpenRenameTitle { item: item.clone() })
        .unwrap();
    let rename = only_effect(controller.dispatch(CatalogAction::ConfirmDialog).unwrap());
    let rename_owner = operation_owner(&rename);
    let stale_rename = match &rename_owner {
        CatalogOperationOwner::RenameTitle { token, target } => {
            CatalogOperationOwner::RenameTitle {
                token: stale_window(*token),
                target: target.clone(),
            }
        }
        other => panic!("expected rename owner, got {other:?}"),
    };
    let before_stale = controller.state().clone();
    controller
        .accept(CatalogResult::OperationFailed {
            owner: stale_rename,
            message: "stale rename".into(),
        })
        .unwrap();
    assert_eq!(controller.state(), &before_stale);
    controller
        .accept(CatalogResult::OperationFailed {
            owner: rename_owner,
            message: "rename failed".into(),
        })
        .unwrap();
    assert!(controller.state().dialog.is_none());

    controller
        .dispatch(CatalogAction::OpenDelete { item })
        .unwrap();
    let delete = only_effect(controller.dispatch(CatalogAction::ConfirmDialog).unwrap());
    let delete_owner = operation_owner(&delete);
    let stale_delete = match &delete_owner {
        CatalogOperationOwner::Delete { token, targets } => CatalogOperationOwner::Delete {
            token: stale_window(*token),
            targets: targets.clone(),
        },
        other => panic!("expected delete owner, got {other:?}"),
    };
    let before_stale = controller.state().clone();
    controller
        .accept(CatalogResult::OperationFailed {
            owner: stale_delete,
            message: "stale delete".into(),
        })
        .unwrap();
    assert_eq!(controller.state(), &before_stale);
    controller
        .accept(CatalogResult::OperationFailed {
            owner: delete_owner,
            message: "delete failed".into(),
        })
        .unwrap();
    assert!(controller.state().dialog.is_none());
}

#[test]
fn upload_detail_uses_the_accepted_title_and_saved_cloud_preferences() {
    let mut controller = controller();
    seed_local(&mut controller, vec![local_item(0)]);
    controller
        .set_cloud_context(
            Some(cloud_owner("account-a", 9)),
            CatalogCloudPreferences {
                default_visibility: CatalogUploadVisibility::Unlisted,
                delete_local_after_upload: true,
            },
        )
        .unwrap();

    let effect = only_effect(
        controller
            .dispatch(CatalogAction::OpenUpload {
                item: local_identity(0),
            })
            .unwrap(),
    );
    let request = match effect {
        CatalogEffect::LoadClipDetail {
            request,
            title,
            description,
            ..
        } => {
            assert_eq!(title, "Clip 00000");
            assert!(description.is_empty());
            request
        }
        other => panic!("expected detail effect, got {other:?}"),
    };
    let detail = ClipDetail::new(
        0,
        Vec::new(),
        "",
        Vec::new(),
        UploadDialogSummary::new("Clip 00000", "", "", "").unwrap(),
    )
    .unwrap();
    controller
        .accept(CatalogResult::ClipDetail(ClipDetailResult::new(
            &request, detail,
        )))
        .unwrap();
    let dialog = controller.state().dialog.as_ref().unwrap();
    assert_eq!(dialog.visibility, Some(CatalogUploadVisibility::Unlisted));
    assert!(dialog.delete_local_after_upload);

    let start = only_effect(controller.dispatch(CatalogAction::ConfirmDialog).unwrap());
    assert!(matches!(
        start,
        CatalogEffect::StartUpload { owner, .. }
            if owner == cloud_owner("account-a", 9)
    ));
}

#[test]
fn replacement_login_invalidates_the_old_upload_dialog_and_fences_the_new_generation() {
    let mut controller = controller();
    seed_local(&mut controller, vec![local_item(0)]);
    controller
        .set_cloud_owner(Some(cloud_owner("account-a", 9)))
        .unwrap();

    let first_detail = only_effect(
        controller
            .dispatch(CatalogAction::OpenUpload {
                item: local_identity(0),
            })
            .unwrap(),
    );
    let first_request = match first_detail {
        CatalogEffect::LoadClipDetail { request, .. } => request,
        other => panic!("expected detail effect, got {other:?}"),
    };
    controller
        .accept(CatalogResult::ClipDetail(ClipDetailResult::new(
            &first_request,
            ClipDetail::new(
                0,
                Vec::new(),
                "",
                Vec::new(),
                UploadDialogSummary::new("Clip 00000", "", "", "").unwrap(),
            )
            .unwrap(),
        )))
        .unwrap();
    assert!(controller.state().dialog.is_some());

    controller
        .set_cloud_owner(Some(cloud_owner("account-a", 10)))
        .unwrap();
    assert!(controller.state().dialog.is_none());
    assert!(controller.dispatch(CatalogAction::ConfirmDialog).is_err());

    let second_detail = only_effect(
        controller
            .dispatch(CatalogAction::OpenUpload {
                item: local_identity(0),
            })
            .unwrap(),
    );
    let second_request = match second_detail {
        CatalogEffect::LoadClipDetail { request, .. } => request,
        other => panic!("expected detail effect, got {other:?}"),
    };
    controller
        .accept(CatalogResult::ClipDetail(ClipDetailResult::new(
            &second_request,
            ClipDetail::new(
                0,
                Vec::new(),
                "",
                Vec::new(),
                UploadDialogSummary::new("Clip 00000", "", "", "").unwrap(),
            )
            .unwrap(),
        )))
        .unwrap();
    let start = only_effect(controller.dispatch(CatalogAction::ConfirmDialog).unwrap());
    assert!(matches!(
        start,
        CatalogEffect::StartUpload { owner, .. }
            if owner == cloud_owner("account-a", 10)
    ));
}

#[test]
fn cloud_review_preparation_releases_stale_leases_and_retains_exact_lease_until_close() {
    let mut controller = controller();
    let item = seed_cloud(&mut controller, "account-a");
    let prepare = only_effect(
        controller
            .dispatch(CatalogAction::OpenItem { item: item.clone() })
            .unwrap(),
    );
    let exact_owner = match prepare {
        CatalogEffect::PrepareCloudReviewMedia { request } => {
            assert_eq!(request.version, 1);
            assert_eq!(request.expected_size_bytes, Some(1_024));
            request.owner
        }
        other => panic!("expected media preparation, got {other:?}"),
    };
    let stale_owner = CloudReviewMediaOwner::new(
        CloudWorkToken {
            window: stale_window(exact_owner.token.window),
            ..exact_owner.token.clone()
        },
        item.clone(),
    )
    .unwrap();
    let stale_media =
        PreparedCloudReviewMedia::new(r"C:\Cache\stale.mp4", CloudMediaLeaseId::new(11).unwrap())
            .unwrap();
    let before_stale = controller.state().clone();
    let release = only_effect(
        controller
            .accept(CatalogResult::CloudReviewMediaPrepared {
                owner: stale_owner,
                media: stale_media.clone(),
            })
            .unwrap(),
    );
    assert_eq!(
        release,
        CatalogEffect::ReleaseCloudReviewMedia {
            lease_id: stale_media.lease_id
        }
    );
    assert_eq!(controller.state(), &before_stale);

    let exact_media =
        PreparedCloudReviewMedia::new(r"C:\Cache\exact.mp4", CloudMediaLeaseId::new(12).unwrap())
            .unwrap();
    let open = only_effect(
        controller
            .accept(CatalogResult::CloudReviewMediaPrepared {
                owner: exact_owner.clone(),
                media: exact_media.clone(),
            })
            .unwrap(),
    );
    assert_eq!(
        open,
        CatalogEffect::OpenPreparedCloudReview {
            owner: exact_owner,
            media: exact_media.clone(),
        }
    );
    assert_eq!(controller.state().active.as_ref(), Some(&item));

    let close = controller.dispatch(CatalogAction::CloseActive).unwrap();
    assert!(matches!(
        close.as_slice(),
        [
            CatalogEffect::CloseReview { .. },
            CatalogEffect::ReleaseCloudReviewMedia { lease_id }
        ] if *lease_id == exact_media.lease_id
    ));
    assert!(controller.state().active.is_none());
}

#[test]
fn prepared_cloud_media_is_released_when_final_projection_publication_fails() {
    let reservation = Arc::new(ArmedCloudProjectionReservation::default());
    let mut controller =
        CatalogController::with_reservation(Arc::new(TestDays), reservation.clone()).unwrap();
    let item = seed_cloud(&mut controller, "account-a");
    let prepare = only_effect(
        controller
            .dispatch(CatalogAction::OpenItem { item })
            .unwrap(),
    );
    let owner = match prepare {
        CatalogEffect::PrepareCloudReviewMedia { request } => request.owner,
        other => panic!("expected Cloud preparation, got {other:?}"),
    };
    let before = controller.state().clone();
    let media =
        PreparedCloudReviewMedia::new(r"C:\Cache\release.mp4", CloudMediaLeaseId::new(99).unwrap())
            .unwrap();
    reservation.arm();
    assert!(matches!(
        controller
            .accept(CatalogResult::CloudReviewMediaPrepared {
                owner,
                media: media.clone(),
            })
            .unwrap()
            .as_slice(),
        [CatalogEffect::ReleaseCloudReviewMedia { lease_id }]
            if *lease_id == media.lease_id
    ));
    assert_eq!(controller.state(), &before);
}

#[test]
fn cloud_review_detach_closes_and_releases_then_reattach_prepares_a_fresh_lease() {
    let mut controller = controller();
    let item = seed_cloud(&mut controller, "account-a");
    let prepare = only_effect(
        controller
            .dispatch(CatalogAction::OpenItem { item: item.clone() })
            .unwrap(),
    );
    let owner = match prepare {
        CatalogEffect::PrepareCloudReviewMedia { request } => request.owner,
        other => panic!("expected Cloud preparation, got {other:?}"),
    };
    let media =
        PreparedCloudReviewMedia::new(r"C:\Cache\open.mp4", CloudMediaLeaseId::new(77).unwrap())
            .unwrap();
    controller
        .accept(CatalogResult::CloudReviewMediaPrepared {
            owner,
            media: media.clone(),
        })
        .unwrap();

    assert!(matches!(
        controller.detach().unwrap().as_slice(),
        [
            CatalogEffect::CloseReview { .. },
            CatalogEffect::ReleaseCloudReviewMedia { lease_id }
        ] if *lease_id == media.lease_id
    ));
    assert_eq!(controller.state().active.as_ref(), Some(&item));
    assert!(matches!(
        controller
            .attach(
                WindowAttachmentGeneration::new(2),
                ForegroundGeneration::new(102),
            )
            .unwrap()
            .as_slice(),
        [
            CatalogEffect::LoadCloudThumbnail { request: thumbnail },
            CatalogEffect::PrepareCloudReviewMedia { request }
        ]
            if request.owner.item == item
                && request.owner.token.window.attachment == WindowAttachmentGeneration::new(2)
                && thumbnail.owner.token.window.attachment == WindowAttachmentGeneration::new(2)
    ));
}

#[test]
fn rename_completion_migrates_retained_identity_state_and_collision_rolls_back() {
    let mut controller = controller();
    seed_local(
        &mut controller,
        vec![local_item(0), local_item(1), local_item(2)],
    );
    let old = local_identity(0);
    controller
        .dispatch(CatalogAction::OpenItem { item: old.clone() })
        .unwrap();
    controller.dispatch(CatalogAction::EnterSelection).unwrap();
    controller
        .dispatch(CatalogAction::ToggleSelection { item: old.clone() })
        .unwrap();
    controller
        .dispatch(CatalogAction::OpenContext { item: old.clone() })
        .unwrap();
    controller
        .dispatch(CatalogAction::OpenRenameFile { item: old.clone() })
        .unwrap();
    let rename = only_effect(controller.dispatch(CatalogAction::ConfirmDialog).unwrap());
    let token = match rename {
        CatalogEffect::RenameFile { token, .. } => token,
        other => panic!("expected rename-file effect, got {other:?}"),
    };
    // A context request can arrive while the executor owns the rename. Both
    // it and the still-open dialog must be migrated by the atomic completion.
    controller
        .dispatch(CatalogAction::OpenContext { item: old.clone() })
        .unwrap();

    let before_collision = controller.state().clone();
    assert!(controller
        .accept(CatalogResult::RenameCompleted {
            token,
            result: RenamedClipInfo {
                old_path: local_item(0).path,
                path: local_item(1).path,
                name: local_item(1).name,
                title: Some("collision".into()),
                kind: "replay".into(),
            },
        })
        .is_err());
    assert_eq!(controller.state(), &before_collision);

    let renamed_path = r"C:\Clips\Renamed.mp4";
    let renamed = CatalogItemIdentity::Local {
        path: ClipPathIdentity::from_text(renamed_path).unwrap(),
    };
    assert!(controller
        .accept(CatalogResult::RenameCompleted {
            token,
            result: RenamedClipInfo {
                old_path: local_item(0).path,
                path: renamed_path.into(),
                name: "Renamed.mp4".into(),
                title: Some("Renamed title".into()),
                kind: "highlight".into(),
            },
        })
        .unwrap()
        .is_empty());
    assert_eq!(
        controller.state().selected.as_slice(),
        std::slice::from_ref(&renamed)
    );
    assert_eq!(controller.state().active.as_ref(), Some(&renamed));
    assert_eq!(
        controller.state().menu.as_ref().map(|menu| &menu.target),
        Some(&renamed)
    );
    assert!(controller.state().dialog.is_none());
    assert!(controller
        .state()
        .local_items
        .iter()
        .any(|item| item.path == renamed_path
            && item.name == "Renamed.mp4"
            && item.title.as_deref() == Some("Renamed title")
            && item.kind == "highlight"));
    assert!(controller
        .state()
        .projection
        .rows
        .iter()
        .any(|row| row.identity == renamed && row.selected && row.active));
}

#[test]
fn mutation_confirmation_is_single_flight_and_completion_survives_a_source_switch() {
    let mut controller = controller();
    seed_local(&mut controller, vec![local_item(0)]);
    let old = local_identity(0);
    controller
        .dispatch(CatalogAction::OpenRenameFile { item: old })
        .unwrap();
    let rename = only_effect(controller.dispatch(CatalogAction::ConfirmDialog).unwrap());
    let token = match rename {
        CatalogEffect::RenameFile { token, .. } => token,
        other => panic!("expected rename-file effect, got {other:?}"),
    };
    let before_duplicate = controller.state().clone();
    assert!(matches!(
        controller.dispatch(CatalogAction::ConfirmDialog),
        Err(clipline_library::CatalogControllerError::Invalid {
            field: "mutation.pending"
        })
    ));
    assert_eq!(controller.state(), &before_duplicate);

    assert!(controller
        .dispatch(CatalogAction::SetSource {
            source: CatalogSource::Cloud,
        })
        .unwrap()
        .is_empty());
    let renamed_path = r"C:\Clips\Renamed-while-cloud.mp4";
    assert!(controller
        .accept(CatalogResult::RenameCompleted {
            token,
            result: RenamedClipInfo {
                old_path: local_item(0).path,
                path: renamed_path.into(),
                name: "Renamed-while-cloud.mp4".into(),
                title: Some("Renamed while Cloud was visible".into()),
                kind: "highlight".into(),
            },
        })
        .unwrap()
        .is_empty());
    assert!(controller
        .dispatch(CatalogAction::SetSource {
            source: CatalogSource::Local,
        })
        .unwrap()
        .is_empty());
    assert!(controller
        .state()
        .projection
        .rows
        .iter()
        .any(|row| row.path == renamed_path));
}

#[test]
fn delete_report_is_exact_atomic_preserves_failures_and_requests_one_refresh() {
    let mut controller = controller();
    seed_local(
        &mut controller,
        vec![local_item(0), local_item(1), local_item(2)],
    );
    let first = local_identity(0);
    controller
        .dispatch(CatalogAction::OpenItem {
            item: first.clone(),
        })
        .unwrap();
    controller.dispatch(CatalogAction::EnterSelection).unwrap();
    for item in [local_identity(2), local_identity(0), local_identity(1)] {
        controller
            .dispatch(CatalogAction::ToggleSelection { item })
            .unwrap();
    }
    controller
        .dispatch(CatalogAction::OpenDeleteSelection)
        .unwrap();
    let delete = only_effect(controller.dispatch(CatalogAction::ConfirmDialog).unwrap());
    let token = match delete {
        CatalogEffect::Delete { token, .. } => token,
        other => panic!("expected delete effect, got {other:?}"),
    };
    let before_invalid = controller.state().clone();

    let invalid_reports = [
        DeletedClipsReport {
            deleted: vec![local_item(0).path.clone()],
            failed: vec![(local_item(0).path.clone(), "duplicate".into())],
        },
        DeletedClipsReport {
            deleted: vec![local_item(0).path.clone(), local_item(2).path.clone()],
            failed: vec![(r"C:\Clips\Foreign.mp4".into(), "foreign".into())],
        },
        DeletedClipsReport {
            deleted: vec![local_item(0).path.clone(), local_item(2).path.clone()],
            failed: Vec::new(),
        },
    ];
    for report in invalid_reports {
        assert!(controller
            .accept(CatalogResult::DeleteCompleted { token, report })
            .is_err());
        assert_eq!(controller.state(), &before_invalid);
    }

    // Deleted paths intentionally arrive in reverse identity order. Report
    // order is not an authority signal and must not affect membership tests.
    let effects = controller
        .accept(CatalogResult::DeleteCompleted {
            token,
            report: DeletedClipsReport {
                deleted: vec![local_item(2).path, local_item(0).path],
                failed: vec![(local_item(1).path, "locked".into())],
            },
        })
        .unwrap();
    assert!(matches!(
        effects.as_slice(),
        [
            CatalogEffect::CloseReview { .. },
            CatalogEffect::RefreshLocal { .. }
        ]
    ));
    assert_eq!(controller.state().local_items.as_slice(), &[local_item(1)]);
    assert_eq!(controller.state().selected.as_slice(), &[local_identity(1)]);
    assert!(controller.state().active.is_none());
    let dialog = controller.state().dialog.as_ref().unwrap();
    assert_eq!(dialog.kind, CatalogDialogKind::PartialDelete);
    assert!(dialog.message.contains("Deleted 2 of 3"));
    assert!(dialog.message.contains("1 could not be deleted"));
    assert!(controller
        .dispatch(CatalogAction::ConfirmDialog)
        .unwrap()
        .is_empty());
    assert!(controller.state().dialog.is_none());
}

#[test]
fn delete_selection_resolves_sorted_off_page_identities_without_using_visible_rows() {
    let mut controller = controller();
    seed_local(&mut controller, (0..65).map(local_item).collect());
    controller.dispatch(CatalogAction::EnterSelection).unwrap();
    for index in [64, 0] {
        controller
            .dispatch(CatalogAction::ToggleSelection {
                item: local_identity(index),
            })
            .unwrap();
    }
    assert_eq!(controller.state().projection.rows.len(), 60);
    assert_eq!(
        controller
            .state()
            .projection
            .rows
            .iter()
            .filter(|row| {
                row.identity == local_identity(0) || row.identity == local_identity(64)
            })
            .count(),
        1,
        "exactly one selected identity must be off the current page"
    );

    assert!(controller
        .dispatch(CatalogAction::OpenDeleteSelection)
        .unwrap()
        .is_empty());
    controller
        .dispatch(CatalogAction::ToggleSelection {
            item: local_identity(1),
        })
        .unwrap();
    let effect = only_effect(controller.dispatch(CatalogAction::ConfirmDialog).unwrap());
    match effect {
        CatalogEffect::Delete { targets, .. } => assert_eq!(
            targets
                .into_iter()
                .map(|target| target.identity)
                .collect::<Vec<_>>(),
            vec![
                ClipPathIdentity::from_text(&local_item(0).path).unwrap(),
                ClipPathIdentity::from_text(&local_item(64).path).unwrap(),
            ]
        ),
        other => panic!("expected delete effect, got {other:?}"),
    }
}

#[test]
fn upload_projection_retains_exact_tokens_updates_badges_and_maps_cancel_by_index() {
    let mut controller = controller();
    seed_local(
        &mut controller,
        (0..=MAX_UPLOAD_SUMMARIES).map(local_item).collect(),
    );

    for index in 0..=MAX_UPLOAD_SUMMARIES {
        controller
            .accept(CatalogResult::UploadByteProgress {
                token: upload_token(index, index as u64 + 1),
                progress: upload_summary(index, "uploading"),
            })
            .unwrap();
    }

    let projection = &controller.state().projection;
    assert_eq!(projection.uploads.len(), MAX_UPLOAD_SUMMARIES);
    assert_eq!(projection.uploads[0].token, upload_token(1, 2));
    assert_eq!(
        projection.cancel_upload_action(0),
        Some(CatalogAction::CancelUpload {
            token: upload_token(1, 2)
        })
    );
    assert_eq!(projection.cancel_upload_action(MAX_UPLOAD_SUMMARIES), None);
    assert_eq!(
        projection
            .rows
            .iter()
            .find(|row| row.identity == local_identity(1))
            .and_then(|row| row.upload_badge.as_deref()),
        Some("uploading")
    );

    let exact = upload_token(1, 2);
    controller
        .accept(CatalogResult::UploadCompleted {
            token: exact.clone(),
            result: upload_summary(1, "ready"),
        })
        .unwrap();
    let projection = &controller.state().projection;
    assert_eq!(projection.uploads[0].token, exact);
    assert_eq!(projection.uploads[0].summary.upload_status, "ready");
    assert_eq!(
        projection
            .rows
            .iter()
            .find(|row| row.identity == local_identity(1))
            .and_then(|row| row.upload_badge.as_deref()),
        Some("ready")
    );

    let replacement = upload_token(1, 99);
    controller
        .accept(CatalogResult::UploadByteProgress {
            token: replacement.clone(),
            progress: upload_summary(1, "uploading"),
        })
        .unwrap();
    controller
        .dispatch(CatalogAction::OpenContext {
            item: local_identity(1),
        })
        .unwrap();
    assert!(controller
        .dispatch(CatalogAction::OpenCancelUpload {
            token: replacement.clone(),
        })
        .unwrap()
        .is_empty());
    let dialog = controller.state().dialog.as_ref().unwrap();
    assert_eq!(dialog.kind, CatalogDialogKind::CancelUpload);
    assert_eq!(dialog.cancel_upload_token.as_ref(), Some(&replacement));
    assert_eq!(dialog.progress.as_deref(), Some("uploading — 50%"));
    assert_eq!(
        only_effect(controller.dispatch(CatalogAction::ConfirmDialog).unwrap()),
        CatalogEffect::CancelUpload {
            token: replacement.clone()
        }
    );
    let cancel = controller
        .state()
        .projection
        .cancel_upload_action(0)
        .unwrap();
    assert_eq!(
        only_effect(controller.dispatch(cancel).unwrap()),
        CatalogEffect::CancelUpload { token: replacement }
    );
}
