use clipline_library::{
    catalog_result_channel, CatalogResult, CatalogResultPublishOutcome, CatalogRevision,
    ClipDetail, ClipDetailRequest, ClipDetailResult, ClipPathIdentity, CloudAccountGeneration,
    CloudAccountKey, CloudLibraryItem, CloudListPageCompletion, CloudNextPage, CloudPageNumber,
    CloudPageOutcome, CloudWorkToken, DeletedClipsReport, DurableUploadToken, ExpectedResultOwner,
    ForegroundGeneration, LocalClipId, LocalClipItem, LocalIndexCompletion, PosterGeneration,
    PosterResult, PosterStatus, PosterWorkToken, RequestGeneration, ResultPortError,
    UploadDialogSummary, UploadGeneration, UploadSummary, WindowAttachmentGeneration,
    WindowWorkToken, CATALOG_RESULT_BYTE_CAPACITY, CATALOG_RESULT_CAPACITY,
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

fn empty_local_index(token: WindowWorkToken, revision: u64) -> CatalogResult {
    CatalogResult::LocalIndex(
        LocalIndexCompletion::new(
            token,
            CatalogRevision::new(revision),
            false,
            Vec::new(),
            Vec::new(),
        )
        .unwrap(),
    )
}

fn local_item(index: usize, size_mb: f64) -> LocalClipItem {
    LocalClipItem {
        path: format!(r"C:\Clips\{index}.mp4"),
        name: format!("{index}.mp4"),
        title: None,
        kind: "replay".into(),
        session: None,
        size_mb,
        modified_unix: index as u64,
        duration_s: Some(1.0),
        marker_count: 0,
        game: None,
        marker_summary: Default::default(),
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

fn cloud_item(id: &str) -> CloudLibraryItem {
    CloudLibraryItem {
        remote_clip_id: id.into(),
        local_clip_id: None,
        path: String::new(),
        title: format!("Clip {id}"),
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

fn cloud_page(token: CloudWorkToken, page: u32, item_count: usize) -> CatalogResult {
    let items = (0..item_count)
        .map(|index| cloud_item(&format!("remote-{page}-{index}")))
        .collect();
    CatalogResult::CloudPage(
        CloudListPageCompletion::page(
            token,
            CatalogRevision::new(1),
            CloudPageNumber::new(page).unwrap(),
            items,
            Vec::new(),
        )
        .unwrap(),
    )
}

fn clip_detail(request: &ClipDetailRequest, digest: &str) -> CatalogResult {
    let upload = UploadDialogSummary::new("", "", "", "").unwrap();
    let detail = ClipDetail::new(0, Vec::new(), digest, Vec::new(), upload).unwrap();
    CatalogResult::ClipDetail(ClipDetailResult::new(request, detail))
}

fn large_local_index(token: WindowWorkToken, revision: u64) -> CatalogResult {
    let field = "x".repeat(1_024);
    let items = (0..1_600)
        .map(|index| LocalClipItem {
            path: format!(r"C:\{index}\{field}.mp4"),
            name: field.clone(),
            title: Some(field.clone()),
            kind: field.clone(),
            session: Some(field.clone()),
            size_mb: 1.0,
            modified_unix: index,
            duration_s: Some(1.0),
            marker_count: 0,
            game: Some(clipline_library::ClipGame {
                id: field.clone(),
                name: field.clone(),
            }),
            marker_summary: Default::default(),
        })
        .collect();
    CatalogResult::LocalIndex(
        LocalIndexCompletion::new(
            token,
            CatalogRevision::new(revision),
            false,
            items,
            Vec::new(),
        )
        .unwrap(),
    )
}

#[test]
fn coalescable_results_replace_only_the_same_exact_key_before_a_barrier() {
    let (sender, receiver) = catalog_result_channel();
    let token = window(3);
    assert_eq!(
        sender.try_send(
            empty_local_index(token, 1),
            ExpectedResultOwner::Window(token)
        ),
        Ok(CatalogResultPublishOutcome::Queued)
    );
    assert_eq!(
        sender.try_send(
            empty_local_index(token, 2),
            ExpectedResultOwner::Window(token)
        ),
        Ok(CatalogResultPublishOutcome::Replaced)
    );
    assert_eq!(receiver.len(), 1);
    assert!(matches!(
        receiver.try_recv(),
        Some(CatalogResult::LocalIndex(completion)) if completion.revision == CatalogRevision::new(2)
    ));

    sender
        .try_send(
            empty_local_index(token, 3),
            ExpectedResultOwner::Window(token),
        )
        .unwrap();
    sender
        .try_send(
            CatalogResult::DeleteCompleted {
                token,
                report: DeletedClipsReport::default(),
            },
            ExpectedResultOwner::Window(token),
        )
        .unwrap();
    assert_eq!(
        sender.try_send(
            empty_local_index(token, 4),
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
fn clip_detail_replacement_is_scoped_to_the_exact_item_and_window() {
    let (sender, receiver) = catalog_result_channel();
    let request = ClipDetailRequest::new(
        ClipPathIdentity::from_text(r"C:\Clips\One.mp4").unwrap(),
        window(40),
    );
    let owner = request.owner().clone();
    sender
        .try_send(
            clip_detail(&request, "first"),
            ExpectedResultOwner::Detail(owner.clone()),
        )
        .unwrap();
    assert_eq!(
        sender.try_send(
            clip_detail(&request, "replacement"),
            ExpectedResultOwner::Detail(owner),
        ),
        Ok(CatalogResultPublishOutcome::Replaced)
    );
    assert!(matches!(
        receiver.try_recv(),
        Some(CatalogResult::ClipDetail(result))
            if result.detail().marker_digest() == "replacement"
    ));
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
            empty_local_index(old_window, 1),
            ExpectedResultOwner::Window(new_window),
        ),
        Err(ResultPortError::Stale)
    );

    let old_cloud = cloud("account-a", 1, 3);
    let new_cloud = cloud("account-b", 1, 3);
    let result = cloud_page(old_cloud, 1, 0);
    assert_eq!(
        sender.try_send(result, ExpectedResultOwner::Cloud(new_cloud)),
        Err(ResultPortError::AccountChanged)
    );

    drop(receiver);
    assert_eq!(
        sender.try_send(
            empty_local_index(new_window, 1),
            ExpectedResultOwner::Window(new_window),
        ),
        Err(ResultPortError::Disconnected)
    );
}

#[test]
fn cloud_pages_coalesce_only_for_the_same_exact_window_and_account_token() {
    let (sender, receiver) = catalog_result_channel();
    let first = cloud("account-a", 1, 20);
    let replacement = cloud("account-a", 1, 21);
    let other_generation = cloud("account-a", 2, 21);

    sender
        .try_send(
            cloud_page(first.clone(), 1, 59),
            ExpectedResultOwner::Cloud(first.clone()),
        )
        .unwrap();
    assert_eq!(
        sender.try_send(
            cloud_page(first.clone(), 1, 60),
            ExpectedResultOwner::Cloud(first),
        ),
        Ok(CatalogResultPublishOutcome::Replaced)
    );
    sender
        .try_send(
            cloud_page(replacement.clone(), 1, 59),
            ExpectedResultOwner::Cloud(replacement),
        )
        .unwrap();
    sender
        .try_send(
            cloud_page(other_generation.clone(), 1, 59),
            ExpectedResultOwner::Cloud(other_generation),
        )
        .unwrap();

    assert_eq!(receiver.len(), 3);
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
fn aggregate_result_bytes_are_bounded_and_released_on_receive() {
    let (sender, receiver) = catalog_result_channel();
    let first = large_local_index(window(51), 1);
    let second = large_local_index(window(52), 1);
    let third = large_local_index(window(53), 1);
    assert!(first.estimated_byte_size() * 2 < CATALOG_RESULT_BYTE_CAPACITY);
    assert!(first.estimated_byte_size() * 3 > CATALOG_RESULT_BYTE_CAPACITY);

    sender
        .try_send(first, ExpectedResultOwner::Window(window(51)))
        .unwrap();
    sender
        .try_send(second, ExpectedResultOwner::Window(window(52)))
        .unwrap();
    assert_eq!(
        sender.try_send(third.clone(), ExpectedResultOwner::Window(window(53))),
        Err(ResultPortError::ByteCapacity {
            capacity: CATALOG_RESULT_BYTE_CAPACITY,
        })
    );

    assert!(receiver.try_recv().is_some());
    assert_eq!(
        sender.try_send(third, ExpectedResultOwner::Window(window(53))),
        Ok(CatalogResultPublishOutcome::Queued)
    );
}

#[test]
fn queue_byte_cap_charges_spare_string_capacity_not_only_visible_length() {
    let (sender, receiver) = catalog_result_channel();
    let token = window(54);
    let mut message = String::with_capacity(CATALOG_RESULT_BYTE_CAPACITY + 1);
    message.push('x');
    assert_eq!(
        sender.try_send(
            CatalogResult::ForegroundFeedback { token, message },
            ExpectedResultOwner::Window(token),
        ),
        Err(ResultPortError::ByteCapacity {
            capacity: CATALOG_RESULT_BYTE_CAPACITY,
        })
    );
    assert!(receiver.is_empty());
}

#[test]
fn oversized_results_are_rejected_before_the_queue_changes() {
    let (sender, receiver) = catalog_result_channel();
    let token = window(10);
    let result = CatalogResult::LocalIndex(LocalIndexCompletion {
        token,
        revision: CatalogRevision::new(1),
        truncated: false,
        items: (0..=10_000).map(|index| local_item(index, 1.0)).collect(),
        warnings: Vec::new(),
    });
    assert_eq!(
        sender.try_send(result, ExpectedResultOwner::Window(token)),
        Err(ResultPortError::PayloadTooLarge {
            field: "local_index.items",
            actual: 10_001,
            maximum: 10_000,
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
    let result = CatalogResult::LocalIndex(LocalIndexCompletion {
        token,
        revision: CatalogRevision::new(1),
        truncated: false,
        items: vec![invalid_local],
        warnings: Vec::new(),
    });
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
    let result = CatalogResult::CloudPage(CloudListPageCompletion {
        token: cloud_token.clone(),
        revision: CatalogRevision::new(1),
        outcome: CloudPageOutcome::Page {
            page: CloudPageNumber::new(1).unwrap(),
            items: vec![cloud_item],
            next: CloudNextPage::Terminal,
        },
        warnings: Vec::new(),
    });
    assert_eq!(
        sender.try_send(result, ExpectedResultOwner::Cloud(cloud_token)),
        Err(ResultPortError::InvalidPayload {
            field: "cloud.duration_ms"
        })
    );
    assert!(receiver.is_empty());

    sender
        .try_send(
            CatalogResult::LocalIndex(
                LocalIndexCompletion::new(
                    token,
                    CatalogRevision::new(2),
                    false,
                    vec![local_item(2, 1.0)],
                    Vec::new(),
                )
                .unwrap(),
            ),
            ExpectedResultOwner::Window(token),
        )
        .unwrap();
    assert_eq!(receiver.len(), 1);
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
