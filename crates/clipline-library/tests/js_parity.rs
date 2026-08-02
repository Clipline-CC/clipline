use std::{fs, path::Path};

use boa_engine::{Context, Source};
use clipline_library::{
    account_key, gallery_card_preview, marker_digest, merge_cloud_library_entries,
    plain_http_confirmed, poster_runtime_unavailable, reconcile_upload_progress, share_url,
    AssetAlias, ClipPathIdentity, CloudAccountFields, CloudLibraryItem, CloudUploadRecord,
    DeterministicLru, GalleryCardConfig, GalleryCardIconConfig, GalleryCardInput, GalleryCardStat,
    GalleryCardTitleFormat, GalleryCardTitlePolicy, GalleryGroup, GalleryMarker, GalleryPageState,
    GalleryPresentation, GallerySummaryMode, MarkerCategoryPresentation, MarkerKindPresentation,
    MarkerPresentation, PlayerCardSummary, UploadProgressPatch,
};
use serde_json::{json, Value};

fn javascript_context() -> Context {
    let ui = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/clipline-app/ui");
    let mut context = Context::default();
    for name in [
        "presentation-core.js",
        "cloud-core.js",
        "player-core.js",
        "gallery-window-core.js",
    ] {
        let source = fs::read_to_string(ui.join(name)).unwrap();
        context
            .eval(Source::from_bytes(&source))
            .unwrap_or_else(|error| panic!("{name} evaluates: {error}"));
    }
    context
}

#[test]
fn canonical_gallery_window_corpus_matches_javascript() {
    let mut context = javascript_context();
    let js = eval_json(
        &mut context,
        r#"JSON.stringify((() => {
          const groups = [
            { label: 'Today', items: Array.from({ length: 40 }, (_, i) => `t${i}`) },
            { label: 'Yesterday', items: Array.from({ length: 50 }, (_, i) => `y${i}`) },
            { label: 'Earlier', items: Array.from({ length: 70 }, (_, i) => `e${i}`) }
          ];
          let state = GalleryWindowCore.updateState(
            GalleryWindowCore.initialState(), { identity: 'local:grouped', total: 160 }
          );
          const first = GalleryWindowCore.windowGroups(groups, state);
          state = GalleryWindowCore.setPage(state, 1, 160);
          const second = GalleryWindowCore.windowGroups(groups, state);
          const cache = new Map();
          for (let i = 0; i < 500; i += 1) {
            GalleryWindowCore.cacheSet(cache, `poster-${i}`, i % 2 ? 'ready' : 'unavailable', 120);
          }
          const touched = GalleryWindowCore.cacheGet(cache, 'poster-380');
          const evicted = GalleryWindowCore.cacheSet(cache, 'poster-500', 'ready', 120);
          return {
            scale: [50, 500, 2000].map((count) => {
              const items = Array.from({ length: count }, (_, id) => id);
              const sized = GalleryWindowCore.updateState(
                GalleryWindowCore.initialState(), { identity: `local:${count}`, total: count }
              );
              const window = GalleryWindowCore.windowItems(items, sized);
              return [count, window.items.length, window.pageCount, window.start, window.end];
            }),
            first: first.groups.map((g) => [g.label, g.totalCount, g.startInGroup, g.items.length]),
            second: second.groups.map((g) => [g.label, g.totalCount, g.startInGroup, g.items.length]),
            lru: {
              size: cache.size, oldestGone: !cache.has('poster-0'),
              touched, touchedKept: cache.has('poster-380'), evicted
            },
            paths: [
              'C:\\Clips\\One.mp4', 'c:/clips/one.mp4', '\\\\?\\C:\\CLIPS\\ONE.mp4',
              '/clips/One.mp4', '/clips/one.mp4'
            ].map(GalleryWindowCore.clipPathKey),
            ffmpeg: [
              'ffmpeg is not available for poster extraction',
              'FFMPEG IS NOT AVAILABLE FOR POSTER EXTRACTION',
              'ffmpeg poster failed: corrupt input', null
            ].map(GalleryWindowCore.posterRuntimeUnavailable)
          };
        })())"#,
    );

    let scale: Vec<_> = [50_usize, 500, 2_000]
        .into_iter()
        .map(|count| {
            let items: Vec<_> = (0..count).collect();
            let mut state = GalleryPageState::default();
            state.update(format!("local:{count}"), count, None).unwrap();
            let window = state.window_items(&items);
            json!([
                count,
                window.items.len(),
                window.page_count,
                window.start,
                window.end
            ])
        })
        .collect();
    let groups = vec![
        GalleryGroup::new(
            Some("Today".into()),
            (0..40).map(|i| format!("t{i}")).collect(),
        ),
        GalleryGroup::new(
            Some("Yesterday".into()),
            (0..50).map(|i| format!("y{i}")).collect(),
        ),
        GalleryGroup::new(
            Some("Earlier".into()),
            (0..70).map(|i| format!("e{i}")).collect(),
        ),
    ];
    let mut state = GalleryPageState::default();
    state.update("local:grouped", 160, None).unwrap();
    let first = state.window_groups(&groups);
    state.set_page(1, 160);
    let second = state.window_groups(&groups);
    let group_shape = |groups: &[clipline_library::VisibleGalleryGroup<String>]| {
        groups
            .iter()
            .map(|group| {
                json!([
                    group.label,
                    group.total_count,
                    group.start_in_group,
                    group.items.len()
                ])
            })
            .collect::<Vec<_>>()
    };
    let mut cache = DeterministicLru::new(120);
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
    let touched = cache.get("poster-380").copied().unwrap();
    let evicted = cache.insert("poster-500".into(), "ready");
    let paths: Vec<_> = [
        r"C:\Clips\One.mp4",
        "c:/clips/one.mp4",
        r"\\?\C:\CLIPS\ONE.mp4",
        "/clips/One.mp4",
        "/clips/one.mp4",
    ]
    .into_iter()
    .map(|path| {
        ClipPathIdentity::from_text(path)
            .unwrap()
            .as_str()
            .to_owned()
    })
    .collect();
    let rust = json!({
        "scale": scale,
        "first": group_shape(&first.groups),
        "second": group_shape(&second.groups),
        "lru": {
            "size": cache.len(),
            "oldestGone": !cache.contains_key("poster-0"),
            "touched": touched,
            "touchedKept": cache.contains_key("poster-380"),
            "evicted": evicted,
        },
        "paths": paths,
        "ffmpeg": [
            poster_runtime_unavailable("ffmpeg is not available for poster extraction"),
            poster_runtime_unavailable("FFMPEG IS NOT AVAILABLE FOR POSTER EXTRACTION"),
            poster_runtime_unavailable("ffmpeg poster failed: corrupt input"),
            poster_runtime_unavailable(""),
        ],
    });
    assert_eq!(rust, js);
    assert_eq!(
        serde_json::to_vec(&rust).unwrap(),
        serde_json::to_vec(&js).unwrap()
    );
}

#[test]
fn canonical_gallery_presentation_matches_player_core() {
    let mut context = javascript_context();
    let js = eval_json(
        &mut context,
        r#"JSON.stringify((() => {
          const markerPresentation = {
            marker_kinds: {
              ChampionKill: { category: 'hero', glyph: '!' },
              DragonKill: { category: 'objective' }
            },
            marker_categories: {
              hero: { singular: 'hero play', plural: 'hero plays', glyph: '!' },
              objective: { singular: 'map objective', plural: 'map objectives', glyph: '◆' }
            }
          };
          const presentation = {
            data_dragon: { version: '16.13.1' },
            gallery: {
              summary: 'player_summary_kda',
              card: {
                title: 'summary_for_full_session',
                title_format: {
                  type: 'player_summary_stats', separator: ' | ',
                  stats: [{ type: 'kda' }, { type: 'cs_per_min', label: 'CS/min' }]
                },
                icon: {
                  type: 'portrait', source: 'player_summary.champion_name',
                  asset_provider: 'riot_data_dragon_champion_square',
                  asset_key_format: 'data_dragon_champion',
                  asset_aliases: { "vel'koz": 'Velkoz' }
                }
              }
            }
          };
          const clip = { markers: { player_summary: {
            champion_name: "Vel'Koz", kills: 11, deaths: 19, assists: 34,
            creep_score: 204, game_time_s: 1800
          } } };
          return {
            digest: PlayerCore.markerDigest([
              { kind: 'ChampionKill' }, { kind: 'ChampionKill' },
              { kind: 'DragonKill' }, { kind: 'ChampionAssist' }
            ], markerPresentation),
            card: PlayerCore.galleryCardPreview(
              clip, 'session', 'Jun 28 · 12:15 PM', presentation,
              { data_dragon: presentation.data_dragon }
            )
          };
        })())"#,
    );

    let markers = vec![
        GalleryMarker::new("ChampionKill"),
        GalleryMarker::new("ChampionKill"),
        GalleryMarker::new("DragonKill"),
        GalleryMarker::new("ChampionAssist"),
    ];
    let marker_presentation = MarkerPresentation {
        kinds: vec![
            MarkerKindPresentation {
                key: "ChampionKill".into(),
                category: "hero".into(),
                glyph: "!".into(),
            },
            MarkerKindPresentation {
                key: "DragonKill".into(),
                category: "objective".into(),
                glyph: String::new(),
            },
        ],
        categories: vec![
            MarkerCategoryPresentation {
                key: "hero".into(),
                singular: "hero play".into(),
                plural: "hero plays".into(),
                glyph: "!".into(),
                color: None,
            },
            MarkerCategoryPresentation {
                key: "objective".into(),
                singular: "map objective".into(),
                plural: "map objectives".into(),
                glyph: "◆".into(),
                color: None,
            },
        ],
    };
    let presentation = GalleryPresentation {
        summary: GallerySummaryMode::PlayerSummaryKda,
        card: GalleryCardConfig {
            title: GalleryCardTitlePolicy::SummaryForFullSession,
            title_format: Some(GalleryCardTitleFormat {
                separator: " | ".into(),
                stats: vec![
                    GalleryCardStat::Kda,
                    GalleryCardStat::CsPerMin {
                        label: "CS/min".into(),
                    },
                ],
            }),
            icon: Some(GalleryCardIconConfig::Portrait {
                label: String::new(),
                aliases: vec![AssetAlias {
                    alias: "vel'koz".into(),
                    key: "Velkoz".into(),
                }],
            }),
        },
        data_dragon_version: Some("16.13.1".into()),
        ..GalleryPresentation::default()
    };
    let card = gallery_card_preview(
        &GalleryCardInput {
            kind: "session".into(),
            fallback_title: "Jun 28 · 12:15 PM".into(),
            player_summary: Some(PlayerCardSummary {
                champion_name: "Vel'Koz".into(),
                kills: 11,
                deaths: 19,
                assists: 34,
                creep_score: Some(204),
                game_time_s: Some(1_800),
            }),
            ..GalleryCardInput::default()
        },
        &presentation,
    )
    .unwrap();
    let rust = json!({
        "digest": marker_digest(&markers, Some(&marker_presentation)).unwrap(),
        "card": card,
    });
    assert_eq!(rust, js);
    assert_eq!(
        serde_json::to_vec(&rust).unwrap(),
        serde_json::to_vec(&js).unwrap()
    );
}

fn eval_json(context: &mut Context, expression: &str) -> Value {
    let value = context
        .eval(Source::from_bytes(expression))
        .unwrap_or_else(|error| panic!("eval failed: {error}"))
        .to_string(context)
        .unwrap()
        .to_std_string_escaped();
    serde_json::from_str(&value).unwrap()
}

fn upload(
    local_clip_id: &str,
    path: &str,
    remote_clip_id: Option<&str>,
    remote_url: Option<&str>,
    visibility: &str,
    upload_status: &str,
    updated_at_unix: u64,
) -> CloudUploadRecord {
    CloudUploadRecord {
        local_clip_id: local_clip_id.into(),
        path: path.into(),
        remote_clip_id: remote_clip_id.map(str::to_owned),
        remote_url: remote_url.map(str::to_owned),
        visibility: visibility.into(),
        upload_status: upload_status.into(),
        error: None,
        updated_at_unix,
    }
}

#[test]
fn canonical_cloud_corpus_matches_javascript_byte_for_byte_after_normalization() {
    let mut context = javascript_context();
    let js = eval_json(
        &mut context,
        r#"JSON.stringify((() => {
          const account = {
            host_url: 'https://clips.example', connected_user_id: 'user-7',
            credential_target: 'credential-7'
          };
          const current = {
            local_clip_id: 'localKnown', path: 'C:/Clips/local known.mp4',
            remote_clip_id: 'remote-known-old', remote_url: 'https://clips.example/old',
            visibility: 'private', upload_status: 'uploading', error: null,
            updated_at_unix: 10
          };
          const progress = CloudCore.reconcileUploadProgress(current, {
            received_size_bytes: 500, file_size_bytes: 1000
          }, 'unlisted', 20);
          const entries = PlayerCore.cloudLibraryEntries({
            localKnown: current,
            staleHistory: {
              local_clip_id: 'staleHistory', path: 'C:/Clips/stale.mp4',
              remote_clip_id: 'remote-stale', remote_url: 'https://clips.example/stale',
              visibility: 'public', upload_status: 'uploaded_public', updated_at_unix: 30
            },
            active: {
              local_clip_id: 'active', path: '\\\\?\\D:\\Clips\\active.mp4',
              remote_clip_id: 'remote-active', remote_url: null,
              visibility: 'private', upload_status: 'uploaded_processing', updated_at_unix: 25
            }
          }, [{ path: 'C:/Clips/local known.mp4' }, { path: 'D:/Clips/active.mp4' }], [
            {
              remote_clip_id: 'remote-known', local_clip_id: 'localKnown', path: '',
              title: 'Server Known', remote_url: '', visibility: 'private',
              upload_status: 'uploaded_private', updated_at_unix: 40,
              duration_ms: 2500, file_size_bytes: 500
            },
            {
              remote_clip_id: '', local_clip_id: null, path: '', title: 'URL only',
              remote_url: 'https://clips.example/url-only', visibility: 'unlisted',
              upload_status: 'uploaded_public', updated_at_unix: 35
            }
          ], true);
          return {
            accountKey: CloudCore.accountKey(account),
            consent: CloudCore.plainHttpConfirmed('http://clips.local', 'http://clips.local', true),
            progress,
            privateShare: CloudCore.shareUrl(current),
            entries,
          };
        })())"#,
    );

    let current = upload(
        "localKnown",
        "C:/Clips/local known.mp4",
        Some("remote-known-old"),
        Some("https://clips.example/old"),
        "private",
        "uploading",
        10,
    );
    let progress = reconcile_upload_progress(
        &current,
        &UploadProgressPatch {
            received_size_bytes: Some(500),
            file_size_bytes: Some(1_000),
            ..UploadProgressPatch::default()
        },
        "unlisted",
        20,
    )
    .unwrap();
    let uploads = vec![
        current.clone(),
        upload(
            "staleHistory",
            "C:/Clips/stale.mp4",
            Some("remote-stale"),
            Some("https://clips.example/stale"),
            "public",
            "uploaded_public",
            30,
        ),
        upload(
            "active",
            r"\\?\D:\Clips\active.mp4",
            Some("remote-active"),
            None,
            "private",
            "uploaded_processing",
            25,
        ),
    ];
    let cloud = vec![
        CloudLibraryItem {
            remote_clip_id: "remote-known".into(),
            local_clip_id: Some("localKnown".into()),
            path: String::new(),
            title: "Server Known".into(),
            remote_url: String::new(),
            visibility: "private".into(),
            upload_status: "uploaded_private".into(),
            updated_at_unix: 40,
            uploaded_at_unix: None,
            duration_ms: Some(2_500),
            file_size_bytes: Some(500),
            source_type: None,
        },
        CloudLibraryItem {
            remote_clip_id: String::new(),
            local_clip_id: None,
            path: String::new(),
            title: "URL only".into(),
            remote_url: "https://clips.example/url-only".into(),
            visibility: "unlisted".into(),
            upload_status: "uploaded_public".into(),
            updated_at_unix: 35,
            uploaded_at_unix: None,
            duration_ms: None,
            file_size_bytes: None,
            source_type: None,
        },
    ];
    let entries = merge_cloud_library_entries(
        &uploads,
        &[
            "C:/Clips/local known.mp4".into(),
            "D:/Clips/active.mp4".into(),
        ],
        &cloud,
        true,
    )
    .unwrap();
    let rust = json!({
        "accountKey": account_key(&CloudAccountFields {
            host_url: "https://clips.example".into(),
            connected_user_id: "user-7".into(),
            credential_target: "credential-7".into(),
        }).unwrap(),
        "consent": plain_http_confirmed("http://clips.local", "http://clips.local", true),
        "progress": progress,
        "privateShare": share_url(&current),
        "entries": entries,
    });

    assert_eq!(rust, js);
    assert_eq!(
        serde_json::to_vec(&rust).unwrap(),
        serde_json::to_vec(&js).unwrap()
    );
}
