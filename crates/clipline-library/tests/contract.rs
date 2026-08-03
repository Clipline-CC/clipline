use std::path::PathBuf;

use clipline_library::{
    CatalogAction, CatalogEffect, CatalogItemIdentity, CatalogOperationOwner, CatalogPage,
    CatalogResult, CatalogRevision, CatalogSource, CatalogUploadOptions, CatalogUploadProjection,
    CatalogUploadVisibility, ClipDetailRequest, ClipGame, ClipPathIdentity, CloudAccountGeneration,
    CloudAccountKey, CloudAccountSnapshot, CloudCatalogOwner, CloudLibraryItem,
    CloudListPageCompletion, CloudMediaLeaseId, CloudNextPage, CloudPageNumber, CloudPageOutcome,
    CloudReviewMediaOwner, CloudReviewMediaRequest, CloudThumbnailDescriptor, CloudThumbnailOwner,
    CloudThumbnailRequest, CloudWorkToken, DurableUploadToken, ForegroundGeneration,
    GenerationError, LocalClipFilter, LocalClipGrouping, LocalClipId, LocalClipItem, LocalClipSort,
    LocalIndexCompletion, LocalPageIndex, MutationFailure, MutationReport, PayloadBoundsError,
    PosterGeneration, PosterStatus, PreparedCloudReviewMedia, PresentationRow, RemoteClipId,
    RequestGeneration, ResolvedLocalClip, UploadGeneration, UploadSummary,
    WindowAttachmentGeneration, WindowWorkToken, MAX_CATALOG_IDENTITY_BYTES, MAX_CATALOG_PAGE_ROWS,
    MAX_CLOUD_INDEX_ROWS, MAX_CLOUD_SERVER_PAGE, MAX_DECODED_PAGE_IMAGES,
    MAX_LOCAL_INDEX_PAYLOAD_BYTES, MAX_LOCAL_INDEX_ROWS, MAX_MUTATION_ITEMS,
    MAX_MUTATION_PATH_BYTES, MAX_POSTER_RESULT_ENTRIES, MAX_UPLOAD_DESCRIPTION_UTF16,
    MAX_UPLOAD_SUMMARIES, MAX_UPLOAD_TITLE_UTF16,
};

#[test]
fn path_identity_matches_the_shipping_javascript_contract() {
    let equivalent = [
        (r"C:\Clips\One.mp4", r"c:/clips/ONE.mp4"),
        (r"C:\Clips\One.mp4", r"//?/C:/CLIPS/one.mp4"),
        (r"\\server\share\One.mp4", r"//?/UNC/SERVER/SHARE/one.mp4"),
        (r" /clips/One.mp4 ", r"/clips/One.mp4"),
    ];
    for (left, right) in equivalent {
        assert!(ClipPathIdentity::same(left, right), "{left:?} != {right:?}");
    }

    let different = [
        (r"C:\Clips\One.mp4", r"C:\Clips\Two.mp4"),
        (r"\\server\share\One.mp4", r"\\server\other\One.mp4"),
        (r"/clips/One.mp4", r"/clips/one.mp4"),
        (r"clips/one.mp4", r"clips\one.mp4"),
        (r"", r""),
        (r"   ", r""),
        (r"C:\clips\one.mp4", r"/clips/one.mp4"),
    ];
    for (left, right) in different {
        assert!(
            !ClipPathIdentity::same(left, right),
            "{left:?} == {right:?}"
        );
    }
}

#[test]
fn identity_does_not_replace_the_original_io_path() {
    let original = PathBuf::from(r" \\?\C:\Clips\MixedCase.mp4 ");
    let identity = ClipPathIdentity::from_path(&original).expect("valid path identity");
    assert_eq!(identity.as_str(), r"windows:c:\clips\mixedcase.mp4");
    assert_eq!(original, PathBuf::from(r" \\?\C:\Clips\MixedCase.mp4 "));
    assert!(ClipPathIdentity::from_text("").is_none());
    assert!(ClipPathIdentity::from_text("   ").is_none());
}

#[test]
fn all_contract_generations_fail_instead_of_wrapping() {
    macro_rules! assert_exhausts {
        ($type:ty, $name:literal) => {{
            let current = <$type>::new(u64::MAX);
            assert_eq!(
                current.checked_next(),
                Err(GenerationError::Exhausted { counter: $name })
            );
            assert_eq!(current.get(), u64::MAX);
        }};
    }

    assert_exhausts!(CatalogRevision, "catalog_revision");
    assert_exhausts!(RequestGeneration, "request_generation");
    assert_exhausts!(ForegroundGeneration, "foreground_generation");
    assert_exhausts!(CloudAccountGeneration, "cloud_account_generation");
    assert_exhausts!(WindowAttachmentGeneration, "window_attachment_generation");
    assert_exhausts!(UploadGeneration, "upload_generation");
    assert_exhausts!(PosterGeneration, "poster_generation");
}

#[test]
fn work_tokens_pin_exact_window_account_and_upload_ownership() {
    let window = WindowWorkToken {
        attachment: WindowAttachmentGeneration::new(3),
        foreground: ForegroundGeneration::new(5),
        request: RequestGeneration::new(8),
    };
    let account_key = CloudAccountKey::new("https://clips.example|user-7").unwrap();
    let cloud = CloudWorkToken {
        window,
        account_key: account_key.clone(),
        account_generation: CloudAccountGeneration::new(13),
    };
    let upload = DurableUploadToken {
        account_key,
        account_generation: CloudAccountGeneration::new(13),
        upload_generation: UploadGeneration::new(21),
        local_clip_id: LocalClipId::new("local-4").unwrap(),
        source_path: ClipPathIdentity::from_text(r"C:\Clips\One.mp4").unwrap(),
    };

    assert_eq!(cloud.window, window);
    let catalog_owner = CloudCatalogOwner::from_work_token(&cloud);
    assert_eq!(catalog_owner.account_key, cloud.account_key);
    assert_eq!(catalog_owner.account_generation, cloud.account_generation);
    let cloud_identity = CatalogItemIdentity::Cloud {
        account_key: catalog_owner.account_key.clone(),
        account_generation: catalog_owner.account_generation,
        remote_clip_id: RemoteClipId::new("remote-7").unwrap(),
    };
    assert!(cloud_identity.matches_cloud_catalog_owner(&catalog_owner));
    assert!(cloud_identity.matches_cloud_owner(&cloud));
    let json = serde_json::to_value(&upload).unwrap();
    assert_eq!(json["account_generation"], 13);
    assert_eq!(json["upload_generation"], 21);
    assert_eq!(json["source_path"], r"windows:c:\clips\one.mp4");
    assert!(json.get("attachment").is_none());
    assert!(json.to_string().find("credential").is_none());
    assert!(json.to_string().find("secret").is_none());

    let round_trip: DurableUploadToken = serde_json::from_value(json).unwrap();
    assert_eq!(round_trip, upload);
}

#[test]
fn deserialization_preserves_identity_invariants() {
    let valid: ClipPathIdentity = serde_json::from_str(r#""windows:c:\\clips\\one.mp4""#).unwrap();
    assert_eq!(valid.as_str(), r"windows:c:\clips\one.mp4");

    for invalid in [
        serde_json::json!(""),
        serde_json::json!("C:\\Clips\\One.mp4"),
        serde_json::json!("windows:C:\\Clips\\One.mp4"),
        serde_json::json!(format!("exact:{}", "x".repeat(16 * 1024 + 1))),
    ] {
        assert!(
            serde_json::from_value::<ClipPathIdentity>(invalid).is_err(),
            "invalid path identity must fail"
        );
    }

    assert!(serde_json::from_value::<CloudAccountKey>(serde_json::json!("   ")).is_err());
    assert!(
        serde_json::from_value::<LocalClipId>(serde_json::json!("x".repeat(16 * 1024 + 1)))
            .is_err()
    );
    assert!(serde_json::from_value::<RemoteClipId>(serde_json::json!("   ")).is_err());
    assert!(serde_json::from_value::<RemoteClipId>(serde_json::json!(
        "x".repeat(MAX_CATALOG_IDENTITY_BYTES + 1)
    ))
    .is_err());
}

#[test]
fn identity_final_encoded_key_is_bounded_and_round_trips_at_the_limit() {
    let exact_raw = "x".repeat(MAX_CATALOG_IDENTITY_BYTES - "exact:".len());
    let exact = ClipPathIdentity::from_text(&exact_raw).unwrap();
    assert_eq!(exact.as_str().len(), MAX_CATALOG_IDENTITY_BYTES);
    let exact_round_trip: ClipPathIdentity =
        serde_json::from_value(serde_json::to_value(&exact).unwrap()).unwrap();
    assert_eq!(exact_round_trip, exact);
    assert!(ClipPathIdentity::from_text(&format!("{exact_raw}x")).is_none());

    let windows_raw = format!(
        r"C:\{}",
        "x".repeat(MAX_CATALOG_IDENTITY_BYTES - "windows:".len() - r"C:\".len())
    );
    let windows = ClipPathIdentity::from_text(&windows_raw).unwrap();
    assert_eq!(windows.as_str().len(), MAX_CATALOG_IDENTITY_BYTES);
    let windows_round_trip: ClipPathIdentity =
        serde_json::from_value(serde_json::to_value(&windows).unwrap()).unwrap();
    assert_eq!(windows_round_trip, windows);
    assert!(ClipPathIdentity::from_text(&format!("{windows_raw}x")).is_none());
}

#[test]
fn dto_json_keeps_shipping_field_names_and_owned_values() {
    let local = LocalClipItem {
        path: r"C:\Clips\One.mp4".into(),
        name: "One.mp4".into(),
        title: Some("A title".into()),
        kind: "replay".into(),
        session: Some("2026-08-02".into()),
        size_mb: 12.5,
        modified_unix: 42,
        duration_s: Some(7.25),
        marker_count: 3,
        game: Some(ClipGame {
            id: "lol".into(),
            name: "League of Legends".into(),
        }),
        file_identity: None,
        marker_summary: Default::default(),
    };
    let local_json = serde_json::to_value(&local).unwrap();
    for field in [
        "path",
        "name",
        "title",
        "kind",
        "session",
        "size_mb",
        "modified_unix",
        "duration_s",
        "marker_count",
        "game",
    ] {
        assert!(local_json.get(field).is_some(), "missing {field}");
    }
    assert!(local_json.get("file_identity").is_none());
    assert_eq!(
        local.path_identity().unwrap().as_str(),
        r"windows:c:\clips\one.mp4"
    );

    let cloud = CloudLibraryItem {
        remote_clip_id: "remote-1".into(),
        local_clip_id: Some("local-1".into()),
        path: r"C:\Clips\One.mp4".into(),
        title: "Cloud title".into(),
        remote_url: "https://clips.example/c/remote-1".into(),
        visibility: "private".into(),
        upload_status: "ready".into(),
        updated_at_unix: 44,
        uploaded_at_unix: Some(43),
        duration_ms: Some(7_250),
        file_size_bytes: Some(13_107_200),
        source_type: Some("replay".into()),
    };
    let cloud_json = serde_json::to_value(&cloud).unwrap();
    assert_eq!(cloud_json["remote_clip_id"], "remote-1");
    assert_eq!(cloud_json["local_clip_id"], "local-1");
    assert_eq!(cloud_json["file_size_bytes"], 13_107_200);

    let account = CloudAccountSnapshot {
        account_key: CloudAccountKey::new("https://clips.example|user-7").unwrap(),
        generation: CloudAccountGeneration::new(2),
        connected: true,
        host_url: "https://clips.example".into(),
        public_url: Some("https://clips.example".into()),
        username: Some("user".into()),
        display_name: Some("User".into()),
        user_id: Some("user-7".into()),
        default_visibility: "private".into(),
        delete_local_after_upload: false,
        auto_upload_rules: false,
    };
    let account_json = serde_json::to_value(&account).unwrap();
    assert_eq!(account_json["account_key"], "https://clips.example|user-7");
    assert!(account_json.get("token").is_none());
    assert!(account_json.get("password").is_none());
}

#[test]
fn page_action_upload_mutation_and_presentation_are_serializable() {
    let upload = UploadSummary {
        local_clip_id: "local-1".into(),
        path: r"C:\Clips\One.mp4".into(),
        upload_status: "uploading".into(),
        received_size_bytes: 5,
        file_size_bytes: 10,
        remote_clip_id: None,
        remote_url: None,
        error: None,
    };
    assert_eq!(
        serde_json::to_value(&upload).unwrap()["received_size_bytes"],
        5
    );

    let action = CatalogAction::OpenItem {
        item: CatalogItemIdentity::Local {
            path: ClipPathIdentity::from_text(r"C:\Clips\One.mp4").unwrap(),
        },
    };
    let action_json = serde_json::to_value(&action).unwrap();
    assert_eq!(action_json["kind"], "open_item");
    assert_eq!(action_json["item"]["source"], "local");
    assert_eq!(action_json["item"]["path"], r"windows:c:\clips\one.mp4");
    assert!(serde_json::from_value::<CatalogAction>(serde_json::json!({
        "kind": "open_item",
        "item": {
            "source": "local",
            "path": r"C:\Clips\One.mp4"
        }
    }))
    .is_err());

    let identity = ClipPathIdentity::from_text(r"C:\Clips\One.mp4").unwrap();
    let report = MutationReport {
        succeeded: vec![identity.clone()],
        failed: vec![MutationFailure {
            path: identity,
            message: "busy".into(),
        }],
    };
    assert_eq!(
        serde_json::to_value(&report).unwrap()["failed"][0]["message"],
        "busy"
    );

    let page = CatalogPage {
        source: CatalogSource::Local,
        revision: CatalogRevision::new(1),
        page: 1,
        page_size: 60,
        total: 1,
        has_next: false,
        truncated: false,
        items: vec![PresentationRow {
            identity: CatalogItemIdentity::Local {
                path: ClipPathIdentity::from_text(r"C:\Clips\One.mp4").unwrap(),
            },
            path: r"C:\Clips\One.mp4".into(),
            title: "One".into(),
            subtitle: "Today".into(),
            duration: "0:07".into(),
            kind: "replay".into(),
            selected: false,
            active: true,
            game_badge: None,
            marker_badge: None,
            outcome_badge: None,
            upload_badge: None,
            poster: clipline_library::PresentationPoster::Missing,
            warning: None,
        }],
        warnings: Vec::new(),
    };
    let round_trip: CatalogPage<PresentationRow> =
        serde_json::from_value(serde_json::to_value(page).unwrap()).unwrap();
    assert_eq!(round_trip.items[0].title, "One");
}

#[test]
fn upload_projection_pairs_the_exact_durable_token_with_its_summary() {
    let token = DurableUploadToken {
        account_key: CloudAccountKey::new("https://clips.example|user-7").unwrap(),
        account_generation: CloudAccountGeneration::new(13),
        upload_generation: UploadGeneration::new(21),
        local_clip_id: LocalClipId::new("local-1").unwrap(),
        source_path: ClipPathIdentity::from_text(r"C:\Clips\One.mp4").unwrap(),
    };
    let summary = UploadSummary {
        local_clip_id: "local-1".into(),
        path: r"C:\Clips\One.mp4".into(),
        upload_status: "uploading".into(),
        received_size_bytes: 5,
        file_size_bytes: 10,
        remote_clip_id: None,
        remote_url: None,
        error: None,
    };

    let projection = CatalogUploadProjection::new(token.clone(), summary.clone()).unwrap();
    assert_eq!(projection.token, token);
    assert_eq!(projection.summary, summary);
    assert_eq!(
        serde_json::to_value(&projection).unwrap()["token"]["upload_generation"],
        21
    );

    let mut mismatched = projection.summary.clone();
    mismatched.local_clip_id = "other".into();
    assert_eq!(
        CatalogUploadProjection::new(projection.token.clone(), mismatched),
        Err(PayloadBoundsError::Invalid {
            field: "upload.local_clip_id_mismatch"
        })
    );
}

#[test]
fn public_contract_bounds_are_consistent() {
    assert_eq!(MAX_CATALOG_PAGE_ROWS, 60);
    assert_eq!(MAX_DECODED_PAGE_IMAGES, 32);
    assert_eq!(MAX_POSTER_RESULT_ENTRIES, 120);
    assert_eq!(MAX_LOCAL_INDEX_ROWS, 10_000);
    assert_eq!(MAX_CLOUD_INDEX_ROWS, 10_000);
    assert_eq!(MAX_MUTATION_PATH_BYTES, 1024 * 1024);
    assert_eq!(MAX_UPLOAD_SUMMARIES, 16);
    assert_eq!(MAX_CLOUD_SERVER_PAGE, 1_000_000);
}

fn cloud_page_token() -> CloudWorkToken {
    CloudWorkToken {
        window: WindowWorkToken {
            attachment: WindowAttachmentGeneration::new(1),
            foreground: ForegroundGeneration::new(2),
            request: RequestGeneration::new(3),
        },
        account_key: CloudAccountKey::new("https://clips.example|user-1|credential-1").unwrap(),
        account_generation: CloudAccountGeneration::new(4),
    }
}

fn cloud_page_items(count: usize) -> Vec<CloudLibraryItem> {
    (0..count)
        .map(|index| CloudLibraryItem {
            remote_clip_id: format!("remote-{index}"),
            local_clip_id: None,
            path: String::new(),
            title: format!("Clip {index}"),
            remote_url: format!("https://clips.example/c/remote-{index}"),
            visibility: "public".into(),
            upload_status: "uploaded_public".into(),
            updated_at_unix: 1,
            uploaded_at_unix: None,
            duration_ms: Some(1_000),
            file_size_bytes: Some(1_024),
            source_type: Some("replay".into()),
        })
        .collect()
}

fn local_index_item(index: usize) -> LocalClipItem {
    LocalClipItem {
        path: format!(r"C:\Clips\{index}.mp4"),
        name: format!("{index}.mp4"),
        title: None,
        kind: "replay".into(),
        session: None,
        size_mb: 1.0,
        modified_unix: index as u64,
        duration_s: Some(1.0),
        marker_count: 0,
        game: None,
        file_identity: None,
        marker_summary: Default::default(),
    }
}

#[test]
fn cloud_pages_expose_only_conservative_server_paging_truth() {
    let page_one = CloudPageNumber::new(1).unwrap();
    let short = CloudListPageCompletion::page(
        cloud_page_token(),
        CatalogRevision::new(1),
        page_one,
        cloud_page_items(59),
        Vec::new(),
    )
    .unwrap();
    assert!(matches!(
        short.outcome,
        CloudPageOutcome::Page {
            page,
            next: CloudNextPage::Terminal,
            ..
        } if page == page_one
    ));

    let full = CloudListPageCompletion::page(
        cloud_page_token(),
        CatalogRevision::new(2),
        page_one,
        cloud_page_items(60),
        Vec::new(),
    )
    .unwrap();
    assert!(matches!(
        full.outcome,
        CloudPageOutcome::Page {
            next: CloudNextPage::Probe { page },
            ..
        } if page.get() == 2
    ));

    let empty_first = CloudListPageCompletion::page(
        cloud_page_token(),
        CatalogRevision::new(3),
        page_one,
        Vec::new(),
        Vec::new(),
    )
    .unwrap();
    assert!(matches!(
        empty_first.outcome,
        CloudPageOutcome::Page {
            next: CloudNextPage::Terminal,
            ..
        }
    ));
}

#[test]
fn empty_following_page_steps_back_without_inventing_a_total() {
    let page_two = CloudPageNumber::new(2).unwrap();
    assert_eq!(
        CloudListPageCompletion::page(
            cloud_page_token(),
            CatalogRevision::new(1),
            page_two,
            Vec::new(),
            Vec::new(),
        ),
        Err(PayloadBoundsError::Invalid {
            field: "cloud_page.empty_nonfirst"
        })
    );

    let completion = CloudListPageCompletion::past_end(
        cloud_page_token(),
        CatalogRevision::new(1),
        page_two,
        Vec::new(),
    )
    .unwrap();
    assert!(matches!(
        completion.outcome,
        CloudPageOutcome::PastEnd {
            requested_page,
            fallback_page,
        } if requested_page.get() == 2 && fallback_page.get() == 1
    ));
    let json = serde_json::to_value(CatalogResult::CloudPage(completion)).unwrap();
    assert_eq!(json["kind"], "cloud_page");
    assert!(json.to_string().find("total").is_none());
    assert!(json.to_string().find("page_count").is_none());
}

#[test]
fn cloud_page_numbers_and_shapes_fail_closed() {
    assert_eq!(
        CloudPageNumber::new(0),
        Err(PayloadBoundsError::Invalid {
            field: "cloud_page.number"
        })
    );
    assert_eq!(
        CloudPageNumber::new(MAX_CLOUD_SERVER_PAGE + 1),
        Err(PayloadBoundsError::TooLarge {
            field: "cloud_page.number",
            actual: MAX_CLOUD_SERVER_PAGE as usize + 1,
            maximum: MAX_CLOUD_SERVER_PAGE as usize,
        })
    );
    assert!(CloudListPageCompletion::page(
        cloud_page_token(),
        CatalogRevision::new(1),
        CloudPageNumber::new(1).unwrap(),
        cloud_page_items(MAX_CATALOG_PAGE_ROWS + 1),
        Vec::new(),
    )
    .is_err());

    let malformed = CatalogResult::CloudPage(CloudListPageCompletion {
        token: cloud_page_token(),
        revision: CatalogRevision::new(1),
        outcome: CloudPageOutcome::Page {
            page: CloudPageNumber::new(1).unwrap(),
            items: cloud_page_items(59),
            next: CloudNextPage::Probe {
                page: CloudPageNumber::new(2).unwrap(),
            },
        },
        warnings: Vec::new(),
    });
    assert_eq!(
        malformed.validate_bounds(),
        Err(PayloadBoundsError::Invalid {
            field: "cloud_page.next"
        })
    );
}

#[test]
fn action_collection_and_string_bounds_are_enforced() {
    let long_query = CatalogAction::SetQuery {
        query: "x".repeat(16 * 1024 + 1),
    };
    assert_eq!(
        long_query.validate_bounds(),
        Err(PayloadBoundsError::TooLarge {
            field: "query",
            actual: 16 * 1024 + 1,
            maximum: 16 * 1024,
        })
    );
    assert!(serde_json::from_value::<CatalogAction>(serde_json::json!({
        "kind": "set_query",
        "query": "x".repeat(16 * 1024 + 1),
    }))
    .is_err());
    assert!(serde_json::from_value::<CatalogAction>(serde_json::json!({
        "kind": "set_dialog_text",
        "field": "description",
        "value": "x".repeat(16 * 1024 + 1),
    }))
    .is_err());

    assert!(LocalPageIndex::new(0).is_ok());
    assert!(
        LocalPageIndex::new((MAX_LOCAL_INDEX_ROWS / MAX_CATALOG_PAGE_ROWS) as u32 + 1).is_err()
    );

    for action in [
        CatalogAction::SetLocalFilter {
            filter: LocalClipFilter::Marked,
        },
        CatalogAction::SetLocalSort {
            sort: LocalClipSort::Largest,
        },
        CatalogAction::SetLocalGrouping {
            grouping: LocalClipGrouping::Game,
        },
        CatalogAction::PreviousPage,
        CatalogAction::NextPage,
        CatalogAction::EnterSelection,
        CatalogAction::SelectVisiblePage,
        CatalogAction::OpenDeleteSelection,
        CatalogAction::Escape,
    ] {
        assert_eq!(action.validate_bounds(), Ok(()));
        let round_trip: CatalogAction =
            serde_json::from_value(serde_json::to_value(&action).unwrap()).unwrap();
        assert_eq!(round_trip, action);
    }
    assert_eq!(
        serde_json::to_value(CatalogAction::OpenDeleteSelection).unwrap()["kind"],
        "open_delete_selection"
    );
}

#[test]
fn full_local_index_pins_row_numeric_and_aggregate_byte_bounds() {
    let token = cloud_page_token().window;
    let items = (0..MAX_LOCAL_INDEX_ROWS)
        .map(local_index_item)
        .collect::<Vec<_>>();
    let completion = LocalIndexCompletion::new(
        token,
        CatalogRevision::new(7),
        true,
        items.clone(),
        Vec::new(),
    )
    .unwrap();
    assert!(completion.truncated);
    assert_eq!(completion.items.len(), MAX_LOCAL_INDEX_ROWS);
    assert!(completion.estimated_byte_size() <= MAX_LOCAL_INDEX_PAYLOAD_BYTES);
    assert!(matches!(
        LocalIndexCompletion::new(
            token,
            CatalogRevision::new(8),
            false,
            items
                .into_iter()
                .chain(std::iter::once(local_index_item(MAX_LOCAL_INDEX_ROWS)))
                .collect(),
            Vec::new(),
        ),
        Err(PayloadBoundsError::TooLarge {
            field: "local_index.items",
            actual,
            maximum: MAX_LOCAL_INDEX_ROWS,
        }) if actual == MAX_LOCAL_INDEX_ROWS + 1
    ));

    let mut invalid_numeric = local_index_item(0);
    invalid_numeric.size_mb = f64::NAN;
    assert_eq!(
        LocalIndexCompletion::new(
            token,
            CatalogRevision::new(9),
            false,
            vec![invalid_numeric],
            Vec::new(),
        ),
        Err(PayloadBoundsError::Invalid {
            field: "local.size_mb"
        })
    );

    let oversized = (0..1_024)
        .map(|index| {
            let mut item = local_index_item(index);
            item.name = "x".repeat(MAX_CATALOG_IDENTITY_BYTES);
            item
        })
        .collect();
    assert!(matches!(
        LocalIndexCompletion::new(
            token,
            CatalogRevision::new(10),
            false,
            oversized,
            Vec::new(),
        ),
        Err(PayloadBoundsError::TooLarge {
            field: "local_index.payload_bytes",
            actual,
            maximum: MAX_LOCAL_INDEX_PAYLOAD_BYTES,
        }) if actual > MAX_LOCAL_INDEX_PAYLOAD_BYTES
    ));

    let mut huge_capacity_name = String::with_capacity(MAX_LOCAL_INDEX_PAYLOAD_BYTES + 1);
    huge_capacity_name.push('x');
    let mut capacity_item = local_index_item(0);
    capacity_item.name = huge_capacity_name;
    assert!(matches!(
        LocalIndexCompletion::new(
            token,
            CatalogRevision::new(11),
            false,
            vec![capacity_item],
            Vec::new(),
        ),
        Err(PayloadBoundsError::TooLarge {
            field: "local_index.payload_bytes",
            ..
        })
    ));

    let spare_rows = MAX_LOCAL_INDEX_PAYLOAD_BYTES / std::mem::size_of::<LocalClipItem>() + 1;
    let spare_capacity_items = Vec::with_capacity(spare_rows);
    assert!(matches!(
        LocalIndexCompletion::new(
            token,
            CatalogRevision::new(12),
            false,
            spare_capacity_items,
            Vec::new(),
        ),
        Err(PayloadBoundsError::TooLarge {
            field: "local_index.payload_bytes",
            ..
        })
    ));
}

#[test]
fn typed_effects_pin_exact_owners_and_resolved_paths() {
    let cloud = cloud_page_token();
    let window = cloud.window;
    let identity = ClipPathIdentity::from_text(r"C:\Clips\One.mp4").unwrap();
    let target = ResolvedLocalClip::new(identity.clone(), r"C:\Clips\One.mp4").unwrap();
    let request = ClipDetailRequest::new(identity, window);
    let detail = CatalogEffect::LoadClipDetail {
        token: window,
        request: request.clone(),
        target: target.clone(),
        title: "One".into(),
        description: String::new(),
    };
    assert_eq!(detail.validate_bounds(), Ok(()));
    let detail_json = serde_json::to_value(detail).unwrap();
    assert_eq!(detail_json["token"]["request"], 3);
    assert_eq!(detail_json["target"]["path"], r"C:\Clips\One.mp4");

    let stale_window = WindowWorkToken {
        request: RequestGeneration::new(99),
        ..window
    };
    assert_eq!(
        CatalogEffect::LoadClipDetail {
            token: stale_window,
            request,
            target: target.clone(),
            title: "One".into(),
            description: String::new(),
        }
        .validate_bounds(),
        Err(PayloadBoundsError::Invalid {
            field: "clip_detail.owner"
        })
    );

    let cloud_item = CatalogItemIdentity::Cloud {
        account_key: cloud.account_key.clone(),
        account_generation: cloud.account_generation,
        remote_clip_id: RemoteClipId::new("remote-1").unwrap(),
    };
    assert_eq!(
        CatalogEffect::OpenInBrowser {
            token: cloud.clone(),
            item: cloud_item.clone(),
            url: "https://clips.example/c/remote-1".into(),
        }
        .validate_bounds(),
        Ok(())
    );
    let mut stale_cloud = cloud;
    stale_cloud.account_generation = CloudAccountGeneration::new(5);
    assert_eq!(
        CatalogEffect::CopyPublicLink {
            token: stale_cloud,
            item: cloud_item,
            url: "https://clips.example/c/remote-1".into(),
        }
        .validate_bounds(),
        Err(PayloadBoundsError::Invalid {
            field: "cloud_item.owner"
        })
    );

    let valid_options = CatalogUploadOptions {
        title: Some("x".repeat(MAX_UPLOAD_TITLE_UTF16)),
        description: Some("x".repeat(MAX_UPLOAD_DESCRIPTION_UTF16)),
        visibility: CatalogUploadVisibility::Private,
        audio_track_ids: vec!["track-1".into(), "track-2".into()],
        delete_local_after_upload: false,
    };
    assert_eq!(
        CatalogEffect::StartUpload {
            token: window,
            target: target.clone(),
            options: valid_options,
        }
        .validate_bounds(),
        Ok(())
    );
    let duplicate_audio = CatalogUploadOptions {
        title: None,
        description: None,
        visibility: CatalogUploadVisibility::Unlisted,
        audio_track_ids: vec!["same".into(), "same".into()],
        delete_local_after_upload: false,
    };
    assert_eq!(
        CatalogEffect::StartUpload {
            token: window,
            target,
            options: duplicate_audio,
        }
        .validate_bounds(),
        Err(PayloadBoundsError::Invalid {
            field: "upload_options.duplicate_audio_track_id"
        })
    );
}

#[test]
fn fallible_effects_derive_exact_typed_operation_owners() {
    let cloud = cloud_page_token();
    let window = cloud.window;
    let path = ClipPathIdentity::from_text(r"C:\Clips\One.mp4").unwrap();
    let target = ResolvedLocalClip::new(path.clone(), r"C:\Clips\One.mp4").unwrap();
    let revision = CatalogRevision::new(7);
    let page = CloudPageNumber::new(3).unwrap();

    assert_eq!(
        CatalogEffect::RefreshLocal {
            token: window,
            revision,
        }
        .operation_owner(),
        Ok(Some(CatalogOperationOwner::LocalRefresh {
            token: window,
            revision,
        }))
    );
    assert_eq!(
        CatalogEffect::RefreshCloud {
            token: cloud.clone(),
            revision,
            page,
            query: "marked".into(),
        }
        .operation_owner(),
        Ok(Some(CatalogOperationOwner::CloudRefresh {
            token: cloud.clone(),
            revision,
            page,
        }))
    );

    let detail_request = ClipDetailRequest::new(path.clone(), window);
    assert_eq!(
        CatalogEffect::LoadClipDetail {
            token: window,
            request: detail_request.clone(),
            target: target.clone(),
            title: "One".into(),
            description: String::new(),
        }
        .operation_owner(),
        Ok(Some(CatalogOperationOwner::ClipDetail {
            owner: detail_request.owner().clone(),
        }))
    );
    assert_eq!(
        CatalogEffect::RenameTitle {
            token: window,
            target: target.clone(),
            title: "One".into(),
        }
        .operation_owner(),
        Ok(Some(CatalogOperationOwner::RenameTitle {
            token: window,
            target: path.clone(),
        }))
    );
    assert_eq!(
        CatalogEffect::RenameFile {
            token: window,
            target: target.clone(),
            file_name: "One renamed.mp4".into(),
        }
        .operation_owner(),
        Ok(Some(CatalogOperationOwner::RenameFile {
            token: window,
            target: path.clone(),
        }))
    );
    assert_eq!(
        CatalogEffect::Delete {
            token: window,
            targets: vec![target],
        }
        .operation_owner(),
        Ok(Some(CatalogOperationOwner::Delete {
            token: window,
            targets: vec![path],
        }))
    );
}

#[test]
fn cloud_review_media_requires_exact_account_window_item_and_lease() {
    let cloud = cloud_page_token();
    let item = CatalogItemIdentity::Cloud {
        account_key: cloud.account_key.clone(),
        account_generation: cloud.account_generation,
        remote_clip_id: RemoteClipId::new("remote-review").unwrap(),
    };
    let owner = CloudReviewMediaOwner::new(cloud.clone(), item.clone()).unwrap();
    assert_eq!(
        owner.stable_catalog_owner(),
        CloudCatalogOwner::from_work_token(&cloud)
    );

    let request = CloudReviewMediaRequest::new(owner.clone(), 1_731_234_567, Some(4_096)).unwrap();
    let prepare = CatalogEffect::PrepareCloudReviewMedia {
        request: request.clone(),
    };
    assert_eq!(prepare.validate_bounds(), Ok(()));
    assert_eq!(request.version, 1_731_234_567);
    assert_eq!(request.expected_size_bytes, Some(4_096));
    assert_eq!(
        prepare.operation_owner(),
        Ok(Some(CatalogOperationOwner::CloudReviewMedia {
            owner: owner.clone(),
        }))
    );

    let lease_id = CloudMediaLeaseId::new(41).unwrap();
    let media = PreparedCloudReviewMedia::new(r"C:\Cache\remote-review.mp4", lease_id).unwrap();
    assert_eq!(media.lease_id.get(), 41);
    for effect in [
        CatalogEffect::OpenPreparedCloudReview {
            owner: owner.clone(),
            media: media.clone(),
        },
        CatalogEffect::ReleaseCloudReviewMedia { lease_id },
    ] {
        assert_eq!(effect.validate_bounds(), Ok(()));
    }

    let prepared = CatalogResult::CloudReviewMediaPrepared {
        owner: owner.clone(),
        media: media.clone(),
    };
    assert_eq!(prepared.validate_bounds(), Ok(()));
    assert!(prepared.is_barrier());
    let prepared_json = serde_json::to_value(&prepared).unwrap();
    assert_eq!(prepared_json["kind"], "cloud_review_media_prepared");
    assert_eq!(prepared_json["media"]["lease_id"], 41);
    let prepared_round_trip: CatalogResult = serde_json::from_value(prepared_json).unwrap();
    assert_eq!(prepared_round_trip, prepared);

    let mut wrong_account = cloud.clone();
    wrong_account.account_generation = CloudAccountGeneration::new(99);
    assert_eq!(
        CloudReviewMediaOwner::new(wrong_account.clone(), item.clone()),
        Err(PayloadBoundsError::Invalid {
            field: "cloud_item.owner"
        })
    );
    let invalid_prepared = CatalogResult::CloudReviewMediaPrepared {
        owner: CloudReviewMediaOwner {
            token: wrong_account,
            item: item.clone(),
        },
        media: media.clone(),
    };
    assert_eq!(
        invalid_prepared.validate_bounds(),
        Err(PayloadBoundsError::Invalid {
            field: "cloud_item.owner"
        })
    );
    assert_eq!(
        CloudReviewMediaOwner::new(
            cloud.clone(),
            CatalogItemIdentity::Local {
                path: ClipPathIdentity::from_text(r"C:\Clips\One.mp4").unwrap(),
            },
        ),
        Err(PayloadBoundsError::Invalid {
            field: "cloud_item.owner"
        })
    );
    let mut wrong_window = cloud;
    wrong_window.window.request = RequestGeneration::new(99);
    let wrong_window_owner = CloudReviewMediaOwner::new(wrong_window, item).unwrap();
    assert_ne!(wrong_window_owner, owner);

    assert_eq!(
        PreparedCloudReviewMedia::new(" ", lease_id),
        Err(PayloadBoundsError::Invalid {
            field: "cloud_media.path"
        })
    );
    assert_eq!(
        CloudMediaLeaseId::new(0),
        Err(PayloadBoundsError::Invalid {
            field: "cloud_media.lease_id"
        })
    );
    assert!(
        serde_json::from_value::<PreparedCloudReviewMedia>(serde_json::json!({
            "path": r"C:\Cache\remote-review.mp4",
            "lease_id": 0,
        }))
        .is_err()
    );
    assert!(matches!(
        PreparedCloudReviewMedia::new("x".repeat(MAX_CATALOG_IDENTITY_BYTES + 1), lease_id),
        Err(PayloadBoundsError::TooLarge {
            field: "cloud_media.path",
            ..
        })
    ));
}

#[test]
fn cloud_thumbnail_contract_pins_the_exact_account_window_item_and_version() {
    let token = cloud_page_token();
    let item = CatalogItemIdentity::Cloud {
        account_key: token.account_key.clone(),
        account_generation: token.account_generation,
        remote_clip_id: RemoteClipId::new("remote-thumbnail").unwrap(),
    };
    let descriptor = CloudThumbnailDescriptor::new(item.clone(), 123_456).unwrap();
    let owner = CloudThumbnailOwner::new(token.clone(), descriptor.clone()).unwrap();
    let request = CloudThumbnailRequest::new(owner.clone()).unwrap();
    let effect = CatalogEffect::LoadCloudThumbnail {
        request: request.clone(),
    };
    assert_eq!(effect.validate_bounds(), Ok(()));
    assert_eq!(effect.operation_owner(), Ok(None));
    assert_eq!(request.owner.descriptor.version, 123_456);

    let result = CatalogResult::CloudThumbnail {
        owner: owner.clone(),
        status: PosterStatus::Ready {
            path: r"C:\Cache\remote-thumbnail-123456.jpg".into(),
        },
    };
    assert_eq!(result.validate_bounds(), Ok(()));
    assert!(!result.is_barrier());
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["kind"], "cloud_thumbnail");
    assert_eq!(json["owner"]["descriptor"]["version"], 123_456);
    let round_trip: CatalogResult = serde_json::from_value(json).unwrap();
    assert_eq!(round_trip, result);

    assert_eq!(
        CloudThumbnailDescriptor::new(
            CatalogItemIdentity::Local {
                path: ClipPathIdentity::from_text(r"C:\Clips\local.mp4").unwrap(),
            },
            123_456,
        ),
        Err(PayloadBoundsError::Invalid {
            field: "cloud_thumbnail.item",
        })
    );
    let wrong_item = CatalogItemIdentity::Cloud {
        account_key: CloudAccountKey::new("replacement").unwrap(),
        account_generation: token.account_generation,
        remote_clip_id: RemoteClipId::new("remote-thumbnail").unwrap(),
    };
    assert_eq!(
        CloudThumbnailOwner::new(
            token,
            CloudThumbnailDescriptor::new(wrong_item, 123_456).unwrap(),
        ),
        Err(PayloadBoundsError::Invalid {
            field: "cloud_item.owner",
        })
    );
}

#[test]
fn operation_failures_are_bounded_barriers_with_exact_kind_and_owner() {
    let cloud = cloud_page_token();
    let window = cloud.window;
    let path = ClipPathIdentity::from_text(r"C:\Clips\One.mp4").unwrap();
    let owners = [
        CatalogOperationOwner::LocalRefresh {
            token: window,
            revision: CatalogRevision::new(1),
        },
        CatalogOperationOwner::CloudRefresh {
            token: cloud,
            revision: CatalogRevision::new(2),
            page: CloudPageNumber::new(1).unwrap(),
        },
        CatalogOperationOwner::ClipDetail {
            owner: ClipDetailRequest::new(path.clone(), window).owner().clone(),
        },
        CatalogOperationOwner::RenameTitle {
            token: window,
            target: path.clone(),
        },
        CatalogOperationOwner::RenameFile {
            token: window,
            target: path.clone(),
        },
        CatalogOperationOwner::Delete {
            token: window,
            targets: vec![path],
        },
    ];

    for owner in owners {
        assert_eq!(owner.validate_bounds(), Ok(()));
        let failure = CatalogResult::OperationFailed {
            owner,
            message: "operation failed".into(),
        };
        assert_eq!(failure.validate_bounds(), Ok(()));
        assert!(failure.is_barrier());
        let json = serde_json::to_value(&failure).unwrap();
        assert_eq!(json["kind"], "operation_failed");
        let round_trip: CatalogResult = serde_json::from_value(json).unwrap();
        assert_eq!(round_trip, failure);
    }

    assert_eq!(
        CatalogOperationOwner::Delete {
            token: window,
            targets: Vec::new(),
        }
        .validate_bounds(),
        Err(PayloadBoundsError::Invalid {
            field: "operation.delete.targets"
        })
    );
    let too_many = CatalogOperationOwner::Delete {
        token: window,
        targets: vec![
            ClipPathIdentity::from_text(r"C:\Clips\One.mp4").unwrap();
            MAX_MUTATION_ITEMS + 1
        ],
    };
    assert!(matches!(
        too_many.validate_bounds(),
        Err(PayloadBoundsError::TooLarge {
            field: "operation.delete.targets",
            actual,
            maximum: MAX_MUTATION_ITEMS,
        }) if actual == MAX_MUTATION_ITEMS + 1
    ));
    let oversized_message = CatalogResult::OperationFailed {
        owner: CatalogOperationOwner::LocalRefresh {
            token: window,
            revision: CatalogRevision::new(1),
        },
        message: "x".repeat(64 * 1024 + 1),
    };
    assert!(matches!(
        oversized_message.validate_bounds(),
        Err(PayloadBoundsError::TooLarge {
            field: "operation_failure.message",
            ..
        })
    ));
}
