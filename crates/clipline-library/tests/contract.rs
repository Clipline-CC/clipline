use std::path::PathBuf;

use clipline_library::{
    CatalogAction, CatalogPage, CatalogResult, CatalogRevision, CatalogSource, ClipGame,
    ClipPathIdentity, CloudAccountGeneration, CloudAccountKey, CloudAccountSnapshot,
    CloudLibraryItem, CloudListPageCompletion, CloudNextPage, CloudPageNumber, CloudPageOutcome,
    CloudWorkToken, DurableUploadToken, ForegroundGeneration, GenerationError, LocalClipId,
    LocalClipItem, MutationFailure, MutationReport, PayloadBoundsError, PosterGeneration,
    PresentationRow, RequestGeneration, UploadGeneration, UploadSummary,
    WindowAttachmentGeneration, WindowWorkToken, MAX_CATALOG_IDENTITY_BYTES, MAX_CATALOG_PAGE_ROWS,
    MAX_CLOUD_INDEX_ROWS, MAX_CLOUD_SERVER_PAGE, MAX_DECODED_PAGE_IMAGES, MAX_LOCAL_INDEX_ROWS,
    MAX_MUTATION_PATH_BYTES, MAX_POSTER_RESULT_ENTRIES, MAX_UPLOAD_SUMMARIES,
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

    let action = CatalogAction::OpenClip {
        source: CatalogSource::Local,
        path: r"C:\Clips\One.mp4".into(),
    };
    assert_eq!(serde_json::to_value(&action).unwrap()["kind"], "open_clip");

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
            id: "local-1".into(),
            path: r"C:\Clips\One.mp4".into(),
            title: "One".into(),
            subtitle: "Today".into(),
            duration: "0:07".into(),
            kind: "replay".into(),
            selected: false,
            active: true,
            upload_status: None,
            warning: None,
        }],
        warnings: Vec::new(),
    };
    let round_trip: CatalogPage<PresentationRow> =
        serde_json::from_value(serde_json::to_value(page).unwrap()).unwrap();
    assert_eq!(round_trip.items[0].title, "One");
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
    let too_many = CatalogAction::Delete {
        paths: (0..=MAX_LOCAL_INDEX_ROWS)
            .map(|index| format!(r"C:\Clips\{index}.mp4"))
            .collect(),
    };
    assert_eq!(
        too_many.validate_bounds(),
        Err(PayloadBoundsError::TooLarge {
            field: "delete.paths",
            actual: MAX_LOCAL_INDEX_ROWS + 1,
            maximum: MAX_LOCAL_INDEX_ROWS,
        })
    );

    let cross_page = CatalogAction::Delete {
        paths: (0..(MAX_CATALOG_PAGE_ROWS + 1))
            .map(|index| format!(r"C:\Clips\{index}.mp4"))
            .collect(),
    };
    assert_eq!(cross_page.validate_bounds(), Ok(()));

    let per_path_bytes = MAX_MUTATION_PATH_BYTES / MAX_LOCAL_INDEX_ROWS + 1;
    let aggregate_too_large = CatalogAction::Delete {
        paths: (0..MAX_LOCAL_INDEX_ROWS)
            .map(|_| "x".repeat(per_path_bytes))
            .collect(),
    };
    assert_eq!(
        aggregate_too_large.validate_bounds(),
        Err(PayloadBoundsError::TooLarge {
            field: "delete.path_bytes",
            actual: per_path_bytes * MAX_LOCAL_INDEX_ROWS,
            maximum: MAX_MUTATION_PATH_BYTES,
        })
    );

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
}
