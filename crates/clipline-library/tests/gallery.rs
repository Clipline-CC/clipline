use clipline_library::{
    build_local_gallery, poster_runtime_unavailable, ClipGame, DecodedImageWindow,
    DeterministicLru, GalleryGroup, GalleryPageState, LocalClipFilter, LocalClipGrouping,
    LocalClipItem, LocalClipSort, LocalDay, LocalDayResolver, LocalGalleryOptions,
    MAX_CATALOG_PAGE_ROWS, MAX_DECODED_PAGE_IMAGES, MAX_POSTER_RESULT_ENTRIES,
};

fn clip(path: &str, name: &str, modified_unix: u64) -> LocalClipItem {
    LocalClipItem {
        path: path.to_owned(),
        name: name.to_owned(),
        title: None,
        kind: String::new(),
        session: None,
        size_mb: 0.0,
        modified_unix,
        duration_s: Some(3.0),
        marker_count: 0,
        game: None,
        marker_summary: Default::default(),
    }
}

fn game(name: &str) -> ClipGame {
    ClipGame {
        id: name.to_ascii_lowercase().replace(' ', "-"),
        name: name.to_owned(),
    }
}

#[derive(Clone, Copy)]
struct TestDays {
    today_start: u64,
}

impl LocalDayResolver for TestDays {
    fn today_start_unix(&self) -> u64 {
        self.today_start
    }

    fn resolve_day(&self, timestamp: u64) -> LocalDay {
        let day = timestamp / 86_400;
        LocalDay {
            key: format!("day-{day}"),
            label: format!("Day {day}"),
        }
    }
}

#[test]
fn page_state_validates_size_resets_identity_and_clamps_same_identity() {
    assert_eq!(GalleryPageState::new(0).page_size(), MAX_CATALOG_PAGE_ROWS);
    assert_eq!(
        GalleryPageState::new(MAX_CATALOG_PAGE_ROWS + 1).page_size(),
        MAX_CATALOG_PAGE_ROWS
    );
    assert_eq!(GalleryPageState::new(25).page_size(), 25);

    let mut state = GalleryPageState::new(MAX_CATALOG_PAGE_ROWS);
    state.update("local|all|a,b,c", 500, None).unwrap();
    state.set_page(7, 500);
    state.update("local|all|a,b,c", 470, None).unwrap();
    assert_eq!(state.page(), 7);

    state.update("local|marked|a,c", 120, None).unwrap();
    assert_eq!(state.page(), 0);
    state.set_page(999, 61);
    assert_eq!(state.page(), 1);

    state.update("local|marked|a,c", 61, Some(30)).unwrap();
    assert_eq!(state.page(), 0, "a page-size change resets to page zero");
    assert_eq!(state.page_size(), 30);
}

#[test]
fn deserialization_cannot_bypass_page_or_decoded_image_bounds() {
    assert!(
        serde_json::from_value::<GalleryPageState>(serde_json::json!({
            "page": 0,
            "identity": "x",
            "page_size": MAX_CATALOG_PAGE_ROWS + 1
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<GalleryPageState>(serde_json::json!({
            "page": 0,
            "identity": "x".repeat(clipline_library::MAX_CATALOG_STRING_BYTES + 1),
            "page_size": MAX_CATALOG_PAGE_ROWS
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<DecodedImageWindow>(serde_json::json!({
            "start": 0,
            "end": MAX_DECODED_PAGE_IMAGES + 1
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<DecodedImageWindow>(serde_json::json!({
            "start": 3,
            "end": 2
        }))
        .is_err()
    );

    let state = GalleryPageState::new(30);
    let round_trip: GalleryPageState =
        serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
    assert_eq!(round_trip, state);
}

#[test]
fn empty_and_out_of_range_windows_are_safe_and_bounded() {
    let mut state = GalleryPageState::default();
    state.update("empty", 0, None).unwrap();
    let empty = state.window_items::<u32>(&[]);
    assert_eq!(
        (
            empty.page,
            empty.page_count,
            empty.start,
            empty.end,
            empty.items.len()
        ),
        (0, 0, 0, 0, 0)
    );

    let items: Vec<_> = (0..61).collect();
    state.set_page(999, items.len());
    let last = state.window_items(&items);
    assert_eq!(
        (
            last.page,
            last.page_count,
            last.start,
            last.end,
            last.items.len(),
            last.has_next,
        ),
        (1, 2, 60, 61, 1, false)
    );
}

#[test]
fn grouped_windows_preserve_split_boundaries_and_full_counts() {
    let today: Vec<_> = (0..40).map(|index| format!("t{index}")).collect();
    let yesterday: Vec<_> = (0..50).map(|index| format!("y{index}")).collect();
    let earlier: Vec<_> = (0..70).map(|index| format!("e{index}")).collect();
    let groups = vec![
        GalleryGroup::new(Some("Today".into()), today.iter().collect()),
        GalleryGroup::new(Some("Yesterday".into()), yesterday.iter().collect()),
        GalleryGroup::new(Some("Earlier".into()), earlier.iter().collect()),
    ];
    let mut state = GalleryPageState::default();
    state.update("local:grouped", 160, None).unwrap();

    let first = state.window_groups(&groups);
    let first_shape: Vec<_> = first
        .groups
        .iter()
        .map(|group| {
            (
                group.label.as_deref(),
                group.total_count,
                group.start_in_group,
                group.items.len(),
            )
        })
        .collect();
    assert_eq!(
        first_shape,
        vec![(Some("Today"), 40, 0, 40), (Some("Yesterday"), 50, 0, 20)]
    );

    state.set_page(1, 160);
    let second = state.window_groups(&groups);
    let second_shape: Vec<_> = second
        .groups
        .iter()
        .map(|group| {
            (
                group.label.as_deref(),
                group.total_count,
                group.start_in_group,
                group.items.len(),
            )
        })
        .collect();
    assert_eq!(
        second_shape,
        vec![
            (Some("Yesterday"), 50, 20, 30),
            (Some("Earlier"), 70, 0, 30),
        ]
    );
}

#[test]
fn row_and_decoded_image_windows_stay_bounded_at_scale() {
    for count in [50, 500, 2_000] {
        let items: Vec<_> = (0..count).collect();
        let mut state = GalleryPageState::default();
        state.update(format!("local:{count}"), count, None).unwrap();
        let page = state.window_items(&items);
        assert_eq!(page.items.len(), count.min(MAX_CATALOG_PAGE_ROWS));

        let images = DecodedImageWindow::around(page.items.len(), 0, page.items.len(), 8);
        assert!(images.len() <= MAX_DECODED_PAGE_IMAGES);
        assert!(images.end() <= page.items.len());
    }

    let middle = DecodedImageWindow::around(60, 20, 25, 8);
    assert!(middle.start() <= 20);
    assert!(middle.end() >= 45, "every visible row remains retained");
    assert_eq!(middle.len(), MAX_DECODED_PAGE_IMAGES);
}

#[test]
fn poster_cache_is_deterministic_and_counts_negative_results() {
    let mut cache = DeterministicLru::new(MAX_POSTER_RESULT_ENTRIES);
    for index in 0..500 {
        cache.insert(
            format!("poster-{index}"),
            if index % 2 == 0 {
                "unavailable"
            } else {
                "ready"
            },
        );
    }
    assert_eq!(cache.len(), MAX_POSTER_RESULT_ENTRIES);
    assert!(!cache.contains_key("poster-0"));
    assert_eq!(cache.get("poster-380"), Some(&"unavailable"));
    let evicted = cache.insert("poster-500".to_owned(), "ready");
    assert_eq!(evicted, vec!["poster-381".to_owned()]);
    assert!(cache.contains_key("poster-380"));
}

#[test]
fn only_the_exact_missing_ffmpeg_error_is_global() {
    assert!(poster_runtime_unavailable(
        "ffmpeg is not available for poster extraction"
    ));
    assert!(poster_runtime_unavailable(
        "  FFMPEG IS NOT AVAILABLE FOR POSTER EXTRACTION  "
    ));
    assert!(!poster_runtime_unavailable(
        "ffmpeg poster failed: clip named ffmpeg is not available for poster extraction"
    ));
    assert!(!poster_runtime_unavailable(
        "spawn ffmpeg poster: access denied"
    ));
}

fn catalog() -> Vec<LocalClipItem> {
    let mut ranked = clip(r"C:\Clips\B.mp4", "B.mp4", 1_010_000);
    ranked.title = Some("Ranked Win".into());
    ranked.kind = "replay".into();
    ranked.session = Some("S1".into());
    ranked.size_mb = 10.0;
    ranked.marker_count = 2;
    ranked.game = Some(game("League of Legends"));

    let mut session = clip(r"C:\Clips\A.mp4", "A.mp4", 1_010_000);
    session.kind = "session".into();
    session.session = Some("S1".into());
    session.size_mb = 20.0;
    session.game = Some(game("League of Legends"));

    let mut trim = clip(r"C:\Clips\C_trim_1.mp4", "C_trim_1.mp4", 920_000);
    trim.size_mb = 30.0;
    trim.marker_count = 5;

    let mut legacy = clip("/clips/Case.mp4", "session_legacy.mp4", 700_000);
    legacy.kind = "unknown".into();
    legacy.size_mb = 5.0;
    legacy.marker_count = 1;
    legacy.game = Some(game("osu!"));

    vec![ranked, session, trim, legacy]
}

#[test]
fn local_filter_and_search_match_shipping_fields_and_kind_fallback() {
    let clips = catalog();
    let days = TestDays {
        today_start: 1_000_000,
    };

    let replay = build_local_gallery(
        &clips,
        &LocalGalleryOptions {
            filter: LocalClipFilter::Replay,
            ..Default::default()
        },
        &days,
    );
    assert_eq!(replay.items.len(), 1);
    assert_eq!(replay.items[0].path, r"C:\Clips\B.mp4");

    let sessions = build_local_gallery(
        &clips,
        &LocalGalleryOptions {
            filter: LocalClipFilter::Session,
            ..Default::default()
        },
        &days,
    );
    assert_eq!(
        sessions
            .items
            .iter()
            .map(|item| item.path.as_str())
            .collect::<Vec<_>>(),
        vec![r"C:\Clips\A.mp4", "/clips/Case.mp4"]
    );

    let trimmed = build_local_gallery(
        &clips,
        &LocalGalleryOptions {
            filter: LocalClipFilter::Trim,
            ..Default::default()
        },
        &days,
    );
    assert_eq!(trimmed.items[0].path, r"C:\Clips\C_trim_1.mp4");

    let marked = build_local_gallery(
        &clips,
        &LocalGalleryOptions {
            filter: LocalClipFilter::Marked,
            ..Default::default()
        },
        &days,
    );
    assert_eq!(marked.items.len(), 3);

    for query in ["ranked", "b.mp4", "s1", "league OF legends", "OSU!"] {
        let result = build_local_gallery(
            &clips,
            &LocalGalleryOptions {
                query: query.to_owned(),
                ..Default::default()
            },
            &days,
        );
        assert!(!result.items.is_empty(), "search `{query}` should match");
    }
}

#[test]
fn local_search_includes_the_compact_marker_haystack_case_insensitively() {
    let days = TestDays {
        today_start: 1_000_000,
    };
    let mut enriched = clip(r"C:\Clips\Marker.mp4", "Marker.mp4", 1_010_000);
    enriched.marker_summary.search_text =
        "Faker Blue Team Teleport Rabadon's Deathcap Blue Zenith".into();
    let unrelated = clip(r"C:\Clips\Other.mp4", "Other.mp4", 1_000_000);
    let clips = [enriched, unrelated];

    // The compact sidecar projection deliberately extends the shipping JS
    // champion-name search to participant, team, spell, item, and play terms.
    for query in [
        "fAkEr",
        "BLUE TEAM",
        "telePORT",
        "rabadon's deathCAP",
        "blue ZENITH",
    ] {
        let result = build_local_gallery(
            &clips,
            &LocalGalleryOptions {
                query: query.to_owned(),
                ..Default::default()
            },
            &days,
        );
        assert_eq!(
            result
                .items
                .iter()
                .map(|item| item.path.as_str())
                .collect::<Vec<_>>(),
            vec![r"C:\Clips\Marker.mp4"],
            "compact marker search should match `{query}`"
        );
    }
}

#[test]
fn local_sorts_have_explicit_recency_and_path_identity_tie_breakers() {
    let clips = catalog();
    let days = TestDays {
        today_start: 1_000_000,
    };

    let newest = build_local_gallery(&clips, &LocalGalleryOptions::default(), &days);
    assert_eq!(
        newest.items[0].path, r"C:\Clips\A.mp4",
        "equal timestamps sort by normalized ClipPathIdentity"
    );

    let oldest = build_local_gallery(
        &clips,
        &LocalGalleryOptions {
            sort: LocalClipSort::Oldest,
            ..Default::default()
        },
        &days,
    );
    assert_eq!(oldest.items[0].path, "/clips/Case.mp4");

    let largest = build_local_gallery(
        &clips,
        &LocalGalleryOptions {
            sort: LocalClipSort::Largest,
            ..Default::default()
        },
        &days,
    );
    assert_eq!(largest.items[0].size_mb, 30.0);

    let marks = build_local_gallery(
        &clips,
        &LocalGalleryOptions {
            sort: LocalClipSort::Marks,
            ..Default::default()
        },
        &days,
    );
    assert_eq!(marks.items[0].marker_count, 5);
}

#[test]
fn grouping_matches_smart_day_game_session_and_none_rules() {
    let clips = catalog();
    let days = TestDays {
        today_start: 1_000_000,
    };

    let expected = [
        (
            LocalClipGrouping::Smart,
            vec!["Today", "Yesterday", "Earlier this week"],
        ),
        (
            LocalClipGrouping::Game,
            vec!["League of Legends", "No game detected", "osu!"],
        ),
        (LocalClipGrouping::Session, vec!["S1", "Earlier"]),
    ];
    for (grouping, labels) in expected {
        let result = build_local_gallery(
            &clips,
            &LocalGalleryOptions {
                grouping,
                ..Default::default()
            },
            &days,
        );
        assert_eq!(
            result
                .groups
                .iter()
                .map(|group| group.label.as_deref().unwrap_or(""))
                .collect::<Vec<_>>(),
            labels
        );
    }

    let day = build_local_gallery(
        &clips,
        &LocalGalleryOptions {
            grouping: LocalClipGrouping::Day,
            ..Default::default()
        },
        &days,
    );
    assert_eq!(day.groups[0].label.as_deref(), Some("Day 11"));
    assert_eq!(day.groups[0].items.len(), 2);

    let none = build_local_gallery(
        &clips,
        &LocalGalleryOptions {
            grouping: LocalClipGrouping::None,
            ..Default::default()
        },
        &days,
    );
    assert_eq!(none.groups.len(), 1);
    assert_eq!(none.groups[0].label, None);
    assert_eq!(none.groups[0].items, none.items);
}

#[test]
fn grouped_buckets_are_newest_first_even_when_the_flat_sort_is_oldest() {
    let clips = catalog();
    let days = TestDays {
        today_start: 1_000_000,
    };
    for grouping in [
        LocalClipGrouping::Smart,
        LocalClipGrouping::Day,
        LocalClipGrouping::Game,
        LocalClipGrouping::Session,
    ] {
        let result = build_local_gallery(
            &clips,
            &LocalGalleryOptions {
                sort: LocalClipSort::Oldest,
                grouping,
                ..Default::default()
            },
            &days,
        );
        for group in result.groups {
            for pair in group.items.windows(2) {
                assert!(
                    pair[0].modified_unix >= pair[1].modified_unix,
                    "{grouping:?} did not restore newest-first group order"
                );
                if pair[0].modified_unix == pair[1].modified_unix {
                    assert!(
                        pair[0].path_identity() <= pair[1].path_identity(),
                        "{grouping:?} did not apply the path identity tie-break"
                    );
                }
            }
        }
    }
}
