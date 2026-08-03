use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc, Arc, Mutex,
};
use std::time::Duration;

use clipline_library::{
    build_catalog_projection, catalog_result_channel, CatalogAction, CatalogEffect,
    CatalogItemIdentity, CatalogLoadState, CatalogMenuProjection, CatalogOperationOwner,
    CatalogProjectionInput, CatalogProjectionSource, CatalogResult, CatalogRevision,
    CatalogUploadProjection, ClipPathIdentity, CloudAccountGeneration, CloudAccountKey,
    CloudThumbnailDescriptor, CloudThumbnailOwner, CloudThumbnailRequest, CloudWorkToken,
    DurableUploadToken, ExpectedResultOwner, ForegroundGeneration, GalleryPresentation,
    LocalClipFilter, LocalClipId, LocalClipItem, LocalDay, LocalDayResolver, LocalGalleryOptions,
    LocalIndexCompletion, LocalPageIndex, MarkerSidecarSummary, PosterStatus, PresentationError,
    RemoteClipId, RequestGeneration, SystemProjectionReservation, UploadGeneration, UploadSummary,
    WindowAttachmentGeneration, WindowWorkToken, CATALOG_RESULT_CAPACITY,
};
use clipline_slint_spike::catalog::{
    rejected_effect_result, route_ui_intent, CatalogEffectExecutor, CatalogEffectHandler,
    CatalogResultWake, CatalogUiError, CatalogUiIntent,
};

#[derive(Clone, Copy)]
struct Days;

impl LocalDayResolver for Days {
    fn today_start_unix(&self) -> u64 {
        0
    }

    fn resolve_day(&self, timestamp: u64) -> LocalDay {
        LocalDay {
            key: timestamp.to_string(),
            label: "day".into(),
        }
    }
}

fn projection_with_menu() -> Result<clipline_library::CatalogProjection, PresentationError> {
    let identity = CatalogItemIdentity::Local {
        path: ClipPathIdentity::from_text("C:/clips/one.mp4").unwrap(),
    };
    let menu = CatalogMenuProjection {
        target: identity,
        can_review: true,
        can_rename: true,
        can_delete: true,
        can_upload: true,
        can_reveal: true,
        can_open_browser: false,
        can_copy_link: false,
    };
    build_catalog_projection(
        &CatalogProjectionInput {
            revision: CatalogRevision::new(7),
            source: CatalogProjectionSource::Local {
                items: &[],
                options: &LocalGalleryOptions::default(),
                page: LocalPageIndex::new(0).unwrap(),
            },
            gallery: &GalleryPresentation::default(),
            selected: &[],
            selection_mode: false,
            cloud_query: "",
            active: None,
            posters: &Default::default(),
            cloud_posters: &Default::default(),
            menu: Some(&menu),
            dialog: None,
            uploads: &[],
            load_state: CatalogLoadState::Empty,
        },
        &Days,
        &SystemProjectionReservation,
    )
}

fn cloud_thumbnail_effect(
    window: WindowWorkToken,
    request: u64,
) -> (CatalogEffect, CloudThumbnailOwner) {
    let token = CloudWorkToken {
        window,
        account_key: CloudAccountKey::new("account-a").unwrap(),
        account_generation: CloudAccountGeneration::new(8),
    };
    let item = CatalogItemIdentity::Cloud {
        account_key: token.account_key.clone(),
        account_generation: token.account_generation,
        remote_clip_id: RemoteClipId::new(format!("remote-{request}")).unwrap(),
    };
    let descriptor = CloudThumbnailDescriptor::new(item, request).unwrap();
    let owner = CloudThumbnailOwner::new(token, descriptor).unwrap();
    (
        CatalogEffect::LoadCloudThumbnail {
            request: CloudThumbnailRequest::new(owner.clone()).unwrap(),
        },
        owner,
    )
}

#[test]
fn stale_revision_is_rejected_before_routing_any_action() {
    let projection = projection_with_menu().unwrap();
    let error = route_ui_intent(
        &projection,
        6,
        CatalogUiIntent::SetLocalFilter(LocalClipFilter::Replay),
    )
    .unwrap_err();
    assert_eq!(
        error,
        CatalogUiError::StaleRevision {
            actual: 6,
            current: 7,
        }
    );
}

#[test]
fn menu_actions_resolve_the_controller_owned_typed_identity() {
    let projection = projection_with_menu().unwrap();
    let action = route_ui_intent(&projection, 7, CatalogUiIntent::DeleteFromMenu).unwrap();
    assert_eq!(
        action,
        CatalogAction::OpenDelete {
            item: projection.menu.unwrap().target,
        }
    );
}

#[test]
fn menu_cancel_resolves_the_exact_active_upload_token() {
    let mut projection = projection_with_menu().unwrap();
    let source_path = projection
        .menu
        .as_ref()
        .unwrap()
        .target
        .local_path()
        .unwrap()
        .clone();
    let token = DurableUploadToken {
        account_key: CloudAccountKey::new("account").unwrap(),
        account_generation: CloudAccountGeneration::new(4),
        upload_generation: UploadGeneration::new(5),
        local_clip_id: LocalClipId::new("local-one").unwrap(),
        source_path,
    };
    projection.uploads.push(
        CatalogUploadProjection::new(
            token.clone(),
            UploadSummary {
                local_clip_id: "local-one".into(),
                path: "C:/clips/one.mp4".into(),
                upload_status: "uploading".into(),
                received_size_bytes: 50,
                file_size_bytes: 100,
                remote_clip_id: None,
                remote_url: None,
                error: None,
            },
        )
        .unwrap(),
    );
    assert_eq!(
        route_ui_intent(&projection, 7, CatalogUiIntent::CancelUploadFromMenu).unwrap(),
        CatalogAction::OpenCancelUpload {
            token: token.clone()
        }
    );
    projection.uploads[0].summary.upload_status = "uploaded_public".into();
    assert_eq!(
        route_ui_intent(&projection, 7, CatalogUiIntent::CancelUploadFromMenu).unwrap_err(),
        CatalogUiError::NoCancelableUpload
    );
}

#[test]
fn row_and_audio_indices_fail_closed() {
    let projection = projection_with_menu().unwrap();
    assert_eq!(
        route_ui_intent(&projection, 7, CatalogUiIntent::OpenRow { row: 0 }).unwrap_err(),
        CatalogUiError::RowOutOfBounds { index: 0, len: 0 }
    );
    assert_eq!(
        route_ui_intent(
            &projection,
            7,
            CatalogUiIntent::SetUploadAudioTrack {
                row: 0,
                selected: true,
            },
        )
        .unwrap_err(),
        CatalogUiError::AudioTrackOutOfBounds { index: 0, len: 0 }
    );
}

#[test]
fn controller_wrapper_starts_with_a_bounded_empty_projection() {
    let controller =
        clipline_slint_spike::catalog::SlintCatalogController::new(Arc::new(Days)).unwrap();
    let projection = controller.projection();
    assert_eq!(projection.rows.len(), 0);
    assert_eq!(projection.revision, CatalogRevision::INITIAL);
}

#[test]
fn controller_exposes_only_the_current_local_page_to_the_poster_owner() {
    let mut controller =
        clipline_slint_spike::catalog::SlintCatalogController::new(Arc::new(Days)).unwrap();
    let attachment = WindowAttachmentGeneration::new(50);
    let foreground = ForegroundGeneration::new(51);
    let effects = controller.attach(attachment, foreground).unwrap();
    let (token, revision) = effects
        .into_iter()
        .find_map(|effect| match effect {
            CatalogEffect::RefreshLocal { token, revision } => Some((token, revision)),
            _ => None,
        })
        .unwrap();
    let item = LocalClipItem {
        path: "C:/clips/poster.mp4".into(),
        name: "poster.mp4".into(),
        title: Some("Poster".into()),
        kind: "replay".into(),
        session: None,
        size_mb: 1.0,
        modified_unix: 1,
        duration_s: Some(20.0),
        marker_count: 0,
        game: None,
        file_identity: None,
        marker_summary: MarkerSidecarSummary::default(),
    };
    controller
        .accept(CatalogResult::LocalIndex(
            LocalIndexCompletion::new(token, revision, false, vec![item], Vec::new()).unwrap(),
        ))
        .unwrap();

    let page = controller.poster_page().unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(
        page.items[0].native_path.to_string_lossy(),
        "C:/clips/poster.mp4"
    );
    assert_eq!(page.items[0].seek_seconds, 3.0);
    assert_eq!(page.stamp.len(), 1);

    let revision = controller.revision();
    controller
        .dispatch(
            revision,
            CatalogUiIntent::SetSource(clipline_library::CatalogSource::Cloud),
        )
        .unwrap();
    assert!(controller.poster_page().unwrap().items.is_empty());
}

struct FailingHandler;

impl CatalogEffectHandler for FailingHandler {
    fn execute(
        &self,
        _effect: CatalogEffect,
    ) -> Result<Option<clipline_slint_spike::catalog::OwnedCatalogResult>, String> {
        Err("é".repeat(40_000))
    }
}

#[derive(Default)]
struct WakeCounter(AtomicUsize);

impl CatalogResultWake for WakeCounter {
    fn wake(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn bounded_executor_maps_worker_failure_to_the_exact_operation_owner() {
    let (sender, receiver) = catalog_result_channel();
    let wake = Arc::new(WakeCounter::default());
    let executor =
        CatalogEffectExecutor::start(Arc::new(FailingHandler), sender, wake.clone()).unwrap();
    let token = WindowWorkToken {
        attachment: WindowAttachmentGeneration::new(2),
        foreground: ForegroundGeneration::new(3),
        request: RequestGeneration::new(4),
    };
    let revision = CatalogRevision::new(5);
    executor
        .try_submit(CatalogEffect::RefreshLocal { token, revision })
        .unwrap();
    let result = receiver.wait_recv(Duration::from_secs(2)).unwrap().unwrap();
    match result {
        CatalogResult::OperationFailed { owner, message } => {
            assert_eq!(
                owner,
                CatalogOperationOwner::LocalRefresh { token, revision }
            );
            assert!(message.len() <= clipline_library::MAX_FOREGROUND_MESSAGE_BYTES);
            assert!(message.is_char_boundary(message.len()));
        }
        other => panic!("expected operation failure, got {other:?}"),
    }

    let (thumbnail, thumbnail_owner) = cloud_thumbnail_effect(token, 44);
    executor.try_submit(thumbnail).unwrap();
    let result = receiver.wait_recv(Duration::from_secs(2)).unwrap().unwrap();
    assert!(matches!(
        result,
        CatalogResult::CloudThumbnail {
            owner,
            status: PosterStatus::Failed { message },
        } if owner == thumbnail_owner
            && message.len() == clipline_library::MAX_CATALOG_STRING_BYTES
            && message.is_char_boundary(message.len())
    ));
    executor.shutdown().unwrap();
    assert_eq!(wake.0.load(Ordering::SeqCst), 2);
}

struct PanickingHandler;

impl CatalogEffectHandler for PanickingHandler {
    fn execute(
        &self,
        _effect: CatalogEffect,
    ) -> Result<Option<clipline_slint_spike::catalog::OwnedCatalogResult>, String> {
        panic!("injected catalog handler panic")
    }
}

struct WaitingHandler {
    started: mpsc::Sender<()>,
    release: Mutex<mpsc::Receiver<()>>,
}

impl CatalogEffectHandler for WaitingHandler {
    fn execute(
        &self,
        _effect: CatalogEffect,
    ) -> Result<Option<clipline_slint_spike::catalog::OwnedCatalogResult>, String> {
        let _ = self.started.send(());
        self.release
            .lock()
            .map_err(|_| "release lock poisoned".to_owned())?
            .recv()
            .map_err(|_| "release sender dropped".to_owned())?;
        Ok(None)
    }
}

#[test]
fn executor_drop_closes_admission_and_joins_every_worker() {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (sender, _receiver) = catalog_result_channel();
    let executor = CatalogEffectExecutor::start(
        Arc::new(WaitingHandler {
            started: started_tx,
            release: Mutex::new(release_rx),
        }),
        sender,
        Arc::new(WakeCounter::default()),
    )
    .unwrap();
    let token = WindowWorkToken {
        attachment: WindowAttachmentGeneration::new(40),
        foreground: ForegroundGeneration::new(41),
        request: RequestGeneration::new(42),
    };
    executor
        .try_submit(CatalogEffect::RefreshLocal {
            token,
            revision: CatalogRevision::new(43),
        })
        .unwrap();
    started_rx.recv_timeout(Duration::from_secs(1)).unwrap();

    let (dropped_tx, dropped_rx) = mpsc::channel();
    let dropper = std::thread::spawn(move || {
        drop(executor);
        let _ = dropped_tx.send(());
    });
    assert!(dropped_rx.recv_timeout(Duration::from_millis(50)).is_err());
    release_tx.send(()).unwrap();
    dropped_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("executor Drop must join both fixed workers");
    dropper.join().unwrap();
}

#[test]
fn executor_contains_handler_panics_and_keeps_the_worker_pool_alive() {
    let (sender, receiver) = catalog_result_channel();
    let wake = Arc::new(WakeCounter::default());
    let executor =
        CatalogEffectExecutor::start(Arc::new(PanickingHandler), sender, wake.clone()).unwrap();
    let token = WindowWorkToken {
        attachment: WindowAttachmentGeneration::new(20),
        foreground: ForegroundGeneration::new(21),
        request: RequestGeneration::new(22),
    };
    for revision in [CatalogRevision::new(23), CatalogRevision::new(24)] {
        executor
            .try_submit(CatalogEffect::RefreshLocal { token, revision })
            .unwrap();
        let result = receiver.wait_recv(Duration::from_secs(2)).unwrap().unwrap();
        assert!(matches!(
            result,
            CatalogResult::OperationFailed {
                owner: CatalogOperationOwner::LocalRefresh {
                    token: actual,
                    revision: actual_revision,
                },
                message,
            } if actual == token
                && actual_revision == revision
                && message.contains("panicked")
        ));
    }
    executor.shutdown().unwrap();
    assert_eq!(wake.0.load(Ordering::SeqCst), 2);
}

#[test]
fn executor_retains_a_terminal_completion_until_result_capacity_is_available() {
    let (sender, receiver) = catalog_result_channel();
    for index in 0..CATALOG_RESULT_CAPACITY {
        let queued = WindowWorkToken {
            attachment: WindowAttachmentGeneration::new(30),
            foreground: ForegroundGeneration::new(31),
            request: RequestGeneration::new(100 + index as u64),
        };
        sender
            .try_send(
                CatalogResult::ForegroundFeedback {
                    token: queued,
                    message: "queued".into(),
                },
                ExpectedResultOwner::Window(queued),
            )
            .unwrap();
    }
    let wake = Arc::new(WakeCounter::default());
    let executor =
        CatalogEffectExecutor::start(Arc::new(FailingHandler), sender, wake.clone()).unwrap();
    let token = WindowWorkToken {
        attachment: WindowAttachmentGeneration::new(30),
        foreground: ForegroundGeneration::new(31),
        request: RequestGeneration::new(999),
    };
    let revision = CatalogRevision::new(32);
    executor
        .try_submit(CatalogEffect::RefreshLocal { token, revision })
        .unwrap();
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(wake.0.load(Ordering::SeqCst), 0);
    assert!(receiver.try_recv().is_some());

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while wake.0.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(wake.0.load(Ordering::SeqCst), 1);
    let mut exact_failure = false;
    while let Some(result) = receiver.try_recv() {
        exact_failure |= matches!(
            result,
            CatalogResult::OperationFailed {
                owner: CatalogOperationOwner::LocalRefresh {
                    token: actual,
                    revision: actual_revision,
                },
                ..
            } if actual == token && actual_revision == revision
        );
    }
    assert!(exact_failure);
    executor.shutdown().unwrap();
}

#[test]
fn admission_rejection_maps_only_owned_operations_to_exact_failures() {
    let token = WindowWorkToken {
        attachment: WindowAttachmentGeneration::new(40),
        foreground: ForegroundGeneration::new(41),
        request: RequestGeneration::new(42),
    };
    let revision = CatalogRevision::new(43);
    let effect = CatalogEffect::RefreshLocal { token, revision };
    let completion = rejected_effect_result(&effect, "executor queue is full").unwrap();
    assert!(matches!(
        completion.result,
        CatalogResult::OperationFailed {
            owner: CatalogOperationOwner::LocalRefresh {
                token: actual,
                revision: actual_revision,
            },
            ..
        } if actual == token && actual_revision == revision
    ));
    assert!(rejected_effect_result(&CatalogEffect::CloseReview { token }, "full").is_none());

    let cloud_token = CloudWorkToken {
        window: token,
        account_key: CloudAccountKey::new("account-a").unwrap(),
        account_generation: CloudAccountGeneration::new(7),
    };
    let cloud_item = CatalogItemIdentity::Cloud {
        account_key: cloud_token.account_key.clone(),
        account_generation: cloud_token.account_generation,
        remote_clip_id: RemoteClipId::new("remote-1").unwrap(),
    };
    let completion = rejected_effect_result(
        &CatalogEffect::OpenInBrowser {
            token: cloud_token,
            item: cloud_item,
        },
        "executor queue is full",
    )
    .unwrap();
    assert!(matches!(
        completion,
        clipline_slint_spike::catalog::OwnedCatalogResult {
            result: CatalogResult::ForegroundFeedback {
                token: actual,
                message,
            },
            expected: ExpectedResultOwner::Window(expected),
        } if actual == token && expected == token && message == "executor queue is full"
    ));

    let (effect, owner) = cloud_thumbnail_effect(token, 9);
    let completion = rejected_effect_result(&effect, "executor queue is full").unwrap();
    assert!(matches!(
        completion,
        clipline_slint_spike::catalog::OwnedCatalogResult {
            result: CatalogResult::CloudThumbnail {
                owner: actual,
                status: PosterStatus::Failed { message },
            },
            expected: ExpectedResultOwner::CloudThumbnail(expected),
        } if actual == owner && expected == owner && message == "executor queue is full"
    ));
}
