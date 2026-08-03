use clipline_library::{
    catalog_result_channel, CatalogPage, CatalogResult, CatalogResultPublishOutcome,
    CatalogRevision, CatalogSource, ClipPathIdentity, CloudAccountGeneration, CloudAccountKey,
    CloudLibraryItem, CloudWorkToken, DurableUploadToken, ExpectedResultOwner,
    ForegroundGeneration, LocalClipId, LocalClipItem, MutationReport, PosterGeneration,
    PosterResult, PosterStatus, PosterWorkToken, RequestGeneration, ResultPortError,
    UploadGeneration, UploadSummary, WindowAttachmentGeneration, WindowWorkToken,
    CATALOG_RESULT_CAPACITY,
};

fn window(request: u64) -> WindowWorkToken {
    WindowWorkToken {
        attachment: WindowAttachmentGeneration::new(1),
        foreground: ForegroundGeneration::new(2),
        request: RequestGeneration::new(request),
    }
}

fn cloud(account: &str, account_generation: u64, request: u64) -> CloudWorkToken {
    CloudWorkToken {
        window: window(request),
        account_key: CloudAccountKey::new(account).unwrap(),
        account_generation: CloudAccountGeneration::new(account_generation),
    }
}

fn upload(account: &str, account_generation: u64, generation: u64) -> DurableUploadToken {
    DurableUploadToken {
        account_key: CloudAccountKey::new(account).unwrap(),
        account_generation: CloudAccountGeneration::new(account_generation),
        upload_generation: UploadGeneration::new(generation),
        local_clip_id: LocalClipId::new("same-local-id").unwrap(),
        source_path: ClipPathIdentity::from_text(r"C:\Clips\One.mp4").unwrap(),
    }
}

fn empty_local_page(token: WindowWorkToken, page: u32) -> CatalogResult {
    CatalogResult::LocalPage {
        token,
        page: CatalogPage {
            source: CatalogSource::Local,
            revision: CatalogRevision::new(1),
            page,
            page_size: 60,
            total: 0,
            has_next: false,
            truncated: false,
            items: Vec::new(),
            warnings: Vec::new(),
        },
    }
}

fn progress(token: DurableUploadToken, received: u64) -> CatalogResult {
    CatalogResult::UploadByteProgress {
        token,
        progress: UploadSummary {
            local_clip_id: "same-local-id".into(),
            path: r"C:\Clips\One.mp4".into(),
            upload_status: "uploading".into(),
            received_size_bytes: received,
            file_size_bytes: 100,
            remote_clip_id: None,
            remote_url: None,
            error: None,
        },
    }
}

#[test]
fn coalescable_results_replace_only_the_same_exact_key_before_a_barrier() {
    let (sender, receiver) = catalog_result_channel();
    let token = window(3);
    assert_eq!(
        sender.try_send(
            empty_local_page(token, 1),
            ExpectedResultOwner::Window(token)
        ),
        Ok(CatalogResultPublishOutcome::Queued)
    );
    assert_eq!(
        sender.try_send(
            empty_local_page(token, 2),
            ExpectedResultOwner::Window(token)
        ),
        Ok(CatalogResultPublishOutcome::Replaced)
    );
    assert_eq!(receiver.len(), 1);
    assert!(matches!(
        receiver.try_recv(),
        Some(CatalogResult::LocalPage { page, .. }) if page.page == 2
    ));

    sender
        .try_send(
            empty_local_page(token, 3),
            ExpectedResultOwner::Window(token),
        )
        .unwrap();
    sender
        .try_send(
            CatalogResult::MutationCompleted {
                token,
                report: MutationReport::default(),
            },
            ExpectedResultOwner::Window(token),
        )
        .unwrap();
    assert_eq!(
        sender.try_send(
            empty_local_page(token, 4),
            ExpectedResultOwner::Window(token)
        ),
        Ok(CatalogResultPublishOutcome::Queued)
    );
    assert_eq!(
        receiver.len(),
        3,
        "replacement cannot cross mutation barrier"
    );
}

#[test]
fn poster_replacement_is_scoped_to_token_and_path() {
    let (sender, receiver) = catalog_result_channel();
    let window = window(4);
    let path = ClipPathIdentity::from_text(r"C:\Clips\One.mp4").unwrap();
    let token = PosterWorkToken {
        window,
        poster: PosterGeneration::new(1),
        path: path.clone(),
    };
    let poster = |status| CatalogResult::Poster {
        token: token.clone(),
        poster: PosterResult {
            path: path.clone(),
            status,
        },
    };
    sender
        .try_send(
            poster(PosterStatus::Queued),
            ExpectedResultOwner::Poster(token.clone()),
        )
        .unwrap();
    assert_eq!(
        sender.try_send(
            poster(PosterStatus::Ready {
                path: r"C:\Cache\One.jpg".into(),
            }),
            ExpectedResultOwner::Poster(token),
        ),
        Ok(CatalogResultPublishOutcome::Replaced)
    );
    assert_eq!(receiver.len(), 1);
}

#[test]
fn cloud_progress_never_coalesces_across_accounts_or_upload_generations() {
    let (sender, receiver) = catalog_result_channel();
    let account_a_one = upload("account-a", 1, 7);
    let account_a_two = upload("account-a", 1, 8);
    let account_b = upload("account-b", 1, 7);

    sender
        .try_send(
            progress(account_a_one.clone(), 10),
            ExpectedResultOwner::Upload(account_a_one.clone()),
        )
        .unwrap();
    assert_eq!(
        sender.try_send(
            progress(account_a_one.clone(), 20),
            ExpectedResultOwner::Upload(account_a_one),
        ),
        Ok(CatalogResultPublishOutcome::Replaced)
    );
    sender
        .try_send(
            progress(account_a_two.clone(), 30),
            ExpectedResultOwner::Upload(account_a_two),
        )
        .unwrap();
    sender
        .try_send(
            progress(account_b.clone(), 40),
            ExpectedResultOwner::Upload(account_b),
        )
        .unwrap();

    assert_eq!(receiver.len(), 3);
}

#[test]
fn stale_account_changed_and_disconnected_are_distinct() {
    let (sender, receiver) = catalog_result_channel();
    let old_window = window(1);
    let new_window = window(2);
    assert_eq!(
        sender.try_send(
            empty_local_page(old_window, 1),
            ExpectedResultOwner::Window(new_window),
        ),
        Err(ResultPortError::Stale)
    );

    let old_cloud = cloud("account-a", 1, 3);
    let new_cloud = cloud("account-b", 1, 3);
    let result = CatalogResult::CloudPage {
        token: old_cloud,
        page: CatalogPage {
            source: CatalogSource::Cloud,
            revision: CatalogRevision::new(1),
            page: 1,
            page_size: 60,
            total: 0,
            has_next: false,
            truncated: false,
            items: Vec::new(),
            warnings: Vec::new(),
        },
    };
    assert_eq!(
        sender.try_send(result, ExpectedResultOwner::Cloud(new_cloud)),
        Err(ResultPortError::AccountChanged)
    );

    drop(receiver);
    assert_eq!(
        sender.try_send(
            empty_local_page(new_window, 1),
            ExpectedResultOwner::Window(new_window),
        ),
        Err(ResultPortError::Disconnected)
    );
}

#[test]
fn durable_results_fill_the_fixed_capacity_and_never_drop_silently() {
    let (sender, receiver) = catalog_result_channel();
    let token = window(9);
    for index in 0..CATALOG_RESULT_CAPACITY {
        sender
            .try_send(
                CatalogResult::ForegroundFeedback {
                    token,
                    message: format!("message-{index}"),
                },
                ExpectedResultOwner::Window(token),
            )
            .unwrap();
    }
    assert_eq!(receiver.len(), CATALOG_RESULT_CAPACITY);
    assert_eq!(
        sender.try_send(
            CatalogResult::ForegroundFeedback {
                token,
                message: "full".into(),
            },
            ExpectedResultOwner::Window(token),
        ),
        Err(ResultPortError::Full {
            capacity: CATALOG_RESULT_CAPACITY
        })
    );
}

#[test]
fn oversized_results_are_rejected_before_the_queue_changes() {
    let (sender, receiver) = catalog_result_channel();
    let token = window(10);
    let mut result = empty_local_page(token, 1);
    let CatalogResult::LocalPage { page, .. } = &mut result else {
        unreachable!();
    };
    page.page_size = 61;
    assert_eq!(
        sender.try_send(result, ExpectedResultOwner::Window(token)),
        Err(ResultPortError::PayloadTooLarge {
            field: "page_size",
            actual: 61,
            maximum: 60,
        })
    );
    assert!(receiver.is_empty());

    let result = CatalogResult::ForegroundFeedback {
        token,
        message: "x".repeat(64 * 1024 + 1),
    };
    assert_eq!(
        sender.try_send(result, ExpectedResultOwner::Window(token)),
        Err(ResultPortError::PayloadTooLarge {
            field: "foreground_feedback.message",
            actual: 64 * 1024 + 1,
            maximum: 64 * 1024,
        })
    );
    assert!(receiver.is_empty());
}

#[test]
fn inconsistent_pages_and_invalid_numeric_items_fail_closed() {
    let (sender, receiver) = catalog_result_channel();
    let token = window(11);
    let invalid_local = LocalClipItem {
        path: r"C:\Clips\One.mp4".into(),
        name: "One.mp4".into(),
        title: None,
        kind: "replay".into(),
        session: None,
        size_mb: f64::NAN,
        modified_unix: 1,
        duration_s: Some(-1.0),
        marker_count: 0,
        game: None,
        marker_summary: Default::default(),
    };
    let result = CatalogResult::LocalPage {
        token,
        page: CatalogPage {
            source: CatalogSource::Local,
            revision: CatalogRevision::new(1),
            page: 1,
            page_size: 1,
            total: 1,
            has_next: false,
            truncated: false,
            items: vec![invalid_local],
            warnings: Vec::new(),
        },
    };
    assert_eq!(
        sender.try_send(result, ExpectedResultOwner::Window(token)),
        Err(ResultPortError::InvalidPayload {
            field: "local.size_mb"
        })
    );
    assert!(receiver.is_empty());

    let cloud_token = cloud("account-a", 1, 11);
    let cloud_item = CloudLibraryItem {
        remote_clip_id: "remote-1".into(),
        local_clip_id: None,
        path: String::new(),
        title: "One".into(),
        remote_url: "https://clips.example/c/remote-1".into(),
        visibility: "public".into(),
        upload_status: "ready".into(),
        updated_at_unix: 1,
        uploaded_at_unix: None,
        duration_ms: Some(-1),
        file_size_bytes: Some(-2),
        source_type: None,
    };
    let result = CatalogResult::CloudPage {
        token: cloud_token.clone(),
        page: CatalogPage {
            source: CatalogSource::Cloud,
            revision: CatalogRevision::new(1),
            page: 1,
            page_size: 1,
            total: 1,
            has_next: false,
            truncated: false,
            items: vec![cloud_item],
            warnings: Vec::new(),
        },
    };
    assert_eq!(
        sender.try_send(result, ExpectedResultOwner::Cloud(cloud_token)),
        Err(ResultPortError::InvalidPayload {
            field: "cloud.duration_ms"
        })
    );
    assert!(receiver.is_empty());

    let mut result = empty_local_page(token, 1);
    let CatalogResult::LocalPage { page, .. } = &mut result else {
        unreachable!();
    };
    page.page_size = 1;
    page.total = 0;
    page.items.push(LocalClipItem {
        path: r"C:\Clips\One.mp4".into(),
        name: "One.mp4".into(),
        title: None,
        kind: "replay".into(),
        session: None,
        size_mb: 1.0,
        modified_unix: 1,
        duration_s: Some(1.0),
        marker_count: 0,
        game: None,
        marker_summary: Default::default(),
    });
    assert_eq!(
        sender.try_send(result, ExpectedResultOwner::Window(token)),
        Err(ResultPortError::InvalidPayload {
            field: "page.items_exceed_total"
        })
    );
    assert!(receiver.is_empty());
}

#[test]
fn dropping_all_senders_disconnects_the_receiver() {
    let (sender, receiver) = catalog_result_channel();
    let sender_two = sender.clone();
    drop(sender);
    drop(sender_two);
    assert_eq!(
        receiver.wait_recv(std::time::Duration::from_millis(1)),
        Err(ResultPortError::Disconnected)
    );
}

#[test]
fn upload_payload_identity_must_match_its_ownership_token() {
    let (sender, receiver) = catalog_result_channel();
    let token = upload("account-a", 1, 9);
    let wrong_id = CatalogResult::UploadByteProgress {
        token: token.clone(),
        progress: UploadSummary {
            local_clip_id: "different-local-id".into(),
            path: r"C:\Clips\One.mp4".into(),
            upload_status: "uploading".into(),
            received_size_bytes: 1,
            file_size_bytes: 2,
            remote_clip_id: None,
            remote_url: None,
            error: None,
        },
    };
    assert_eq!(
        sender.try_send(wrong_id, ExpectedResultOwner::Upload(token.clone())),
        Err(ResultPortError::InvalidPayload {
            field: "upload.local_clip_id_mismatch"
        })
    );

    let wrong_path = CatalogResult::UploadCompleted {
        token: token.clone(),
        result: UploadSummary {
            local_clip_id: token.local_clip_id.as_str().into(),
            path: r"C:\Clips\Two.mp4".into(),
            upload_status: "uploaded_private".into(),
            received_size_bytes: 2,
            file_size_bytes: 2,
            remote_clip_id: Some("remote-1".into()),
            remote_url: None,
            error: None,
        },
    };
    assert_eq!(
        sender.try_send(wrong_path, ExpectedResultOwner::Upload(token)),
        Err(ResultPortError::InvalidPayload {
            field: "upload.path_mismatch"
        })
    );
    assert!(receiver.is_empty());
}
