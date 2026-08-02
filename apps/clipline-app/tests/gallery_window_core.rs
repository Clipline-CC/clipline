use boa_engine::{Context, Source};
use clipline_library::{
    ClipPathIdentity, DecodedImageWindow, GalleryPageState, MAX_CATALOG_PAGE_ROWS,
    MAX_DECODED_PAGE_IMAGES,
};
use std::fs;
use std::path::Path;

fn context() -> Context {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/gallery-window-core.js");
    let source = fs::read_to_string(path).expect("read ui/gallery-window-core.js");
    let mut context = Context::default();
    context
        .eval(Source::from_bytes(&source))
        .expect("gallery-window-core.js evaluates without DOM or Tauri globals");
    context
}

fn path_comparison_context() -> Context {
    let mut context = Context::default();
    for name in [
        "presentation-core.js",
        "player-core.js",
        "gallery-window-core.js",
    ] {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui").join(name);
        let source =
            fs::read_to_string(path).unwrap_or_else(|error| panic!("read {name}: {error}"));
        context
            .eval(Source::from_bytes(&source))
            .unwrap_or_else(|error| {
                panic!("{name} evaluates without DOM or Tauri globals: {error}")
            });
    }
    context
}

fn eval(context: &mut Context, expression: &str) -> String {
    context
        .eval(Source::from_bytes(expression))
        .unwrap_or_else(|error| panic!("eval `{expression}`: {error}"))
        .to_string(context)
        .expect("stringify result")
        .to_std_string_escaped()
}

#[test]
fn classic_script_explicitly_exports_the_module_global() {
    let mut context = context();
    assert_eq!(
        eval(
            &mut context,
            "String(globalThis.GalleryWindowCore === GalleryWindowCore)",
        ),
        "true"
    );
}

#[test]
fn clip_path_keys_match_windows_paths_without_collapsing_other_paths() {
    let mut context = context();
    assert_eq!(
        eval(
            &mut context,
            r#"JSON.stringify([
              GalleryWindowCore.clipPathKey("C:\\Clips\\One.mp4"),
              GalleryWindowCore.clipPathKey("c:/clips/one.mp4"),
              GalleryWindowCore.clipPathKey("\\\\?\\C:\\CLIPS\\ONE.mp4"),
              GalleryWindowCore.clipPathKey("/clips/One.mp4"),
              GalleryWindowCore.clipPathKey("/clips/one.mp4")
            ])"#,
        ),
        r#"["windows:c:\\clips\\one.mp4","windows:c:\\clips\\one.mp4","windows:c:\\clips\\one.mp4","exact:/clips/One.mp4","exact:/clips/one.mp4"]"#
    );
    assert_eq!(
        ClipPathIdentity::from_text(r"C:\Clips\One.mp4")
            .unwrap()
            .as_str(),
        r"windows:c:\clips\one.mp4"
    );
    assert_eq!(
        ClipPathIdentity::from_text("/clips/One.mp4")
            .unwrap()
            .as_str(),
        "exact:/clips/One.mp4"
    );
}

#[test]
fn gallery_path_keys_stay_equivalent_to_player_path_matching() {
    let mut context = path_comparison_context();
    assert_eq!(
        eval(
            &mut context,
            r#"JSON.stringify([
              ["C:\\Clips\\One.mp4", "c:/clips/ONE.mp4", true],
              ["C:\\Clips\\One.mp4", "//?/C:/CLIPS/one.mp4", true],
              ["\\\\server\\share\\One.mp4", "//?/UNC/SERVER/SHARE/one.mp4", true],
              ["C:\\Clips\\One.mp4", "C:\\Clips\\Two.mp4", false],
              ["\\\\server\\share\\One.mp4", "\\\\server\\other\\One.mp4", false],
              ["/clips/One.mp4", "/clips/One.mp4", true],
              [" /clips/One.mp4 ", "/clips/One.mp4", true],
              ["/clips/One.mp4", "/clips/one.mp4", false],
              ["clips/one.mp4", "clips\\one.mp4", false],
              ["", "", false],
              ["   ", "", false],
              ["", "C:\\Clips\\One.mp4", false],
              ["   ", "/clips/One.mp4", false],
              ["C:\\clips\\one.mp4", "/clips/one.mp4", false],
              ["C:\\clips\\one.mp4", "\\\\server\\share\\one.mp4", false]
            ].filter(([left, right, expected]) => {
              const playerMatches = PlayerCore.sameClipPath(left, right);
              const leftKey = GalleryWindowCore.clipPathKey(left);
              const rightKey = GalleryWindowCore.clipPathKey(right);
              // Production omits empty keys from its Set, so two invalid
              // paths must not become equivalent merely because both map to "".
              const galleryMatches = Boolean(
                leftKey && rightKey && leftKey === rightKey
              );
              return playerMatches !== expected || galleryMatches !== expected;
            }))"#,
        ),
        "[]"
    );
}

#[test]
fn local_and_cloud_library_sizes_never_exceed_the_card_window() {
    let mut context = context();
    assert_eq!(
        eval(
            &mut context,
            "JSON.stringify([50, 500, 2000].map((count) => {\
               const items = Array.from({ length: count }, (_, id) => ({ id }));\
               let state = GalleryWindowCore.updateState(\
                 GalleryWindowCore.initialState(),\
                 { identity: `local:${count}`, total: count }\
               );\
               const local = GalleryWindowCore.windowItems(items, state);\
               state = GalleryWindowCore.updateState(\
                 state,\
                 { identity: `cloud:${count}`, total: count }\
               );\
               const cloud = GalleryWindowCore.windowItems(items, state);\
               return {\
                 count,\
                 localCards: local.items.length,\
                 localImages: local.items.length,\
                 cloudCards: cloud.items.length,\
                 cloudImages: cloud.items.length,\
                 pages: cloud.pageCount\
               };\
             }))",
        ),
        r#"[{"count":50,"localCards":50,"localImages":50,"cloudCards":50,"cloudImages":50,"pages":1},{"count":500,"localCards":60,"localImages":60,"cloudCards":60,"cloudImages":60,"pages":9},{"count":2000,"localCards":60,"localImages":60,"cloudCards":60,"cloudImages":60,"pages":34}]"#
    );
    for count in [50, 500, 2_000] {
        let items: Vec<_> = (0..count).collect();
        let mut state = GalleryPageState::default();
        state.update(format!("local:{count}"), count, None).unwrap();
        let window = state.window_items(&items);
        assert_eq!(window.items.len(), count.min(MAX_CATALOG_PAGE_ROWS));
        let decoded = DecodedImageWindow::around(window.items.len(), 0, 24, 8);
        assert!(decoded.len() <= MAX_DECODED_PAGE_IMAGES);
    }
}

#[test]
fn grouped_windows_preserve_boundaries_and_full_group_counts() {
    let mut context = context();
    assert_eq!(
        eval(
            &mut context,
            "const groups = [\
               { label: 'Today', clips: Array.from({ length: 40 }, (_, i) => `t${i}`) },\
               { label: 'Yesterday', clips: Array.from({ length: 50 }, (_, i) => `y${i}`) },\
               { label: 'Earlier', clips: Array.from({ length: 70 }, (_, i) => `e${i}`) }\
             ];\
             let state = GalleryWindowCore.updateState(\
               GalleryWindowCore.initialState(),\
               { identity: 'local:grouped', total: 160 }\
             );\
             const first = GalleryWindowCore.windowGroups(groups, state);\
             state = GalleryWindowCore.setPage(state, 1, 160);\
             const second = GalleryWindowCore.windowGroups(groups, state);\
             JSON.stringify({\
               first: first.groups.map((g) => [g.label, g.totalCount, g.startInGroup, g.items.length]),\
               second: second.groups.map((g) => [g.label, g.totalCount, g.startInGroup, g.items.length]),\
               bounded: first.groups.reduce((n, g) => n + g.items.length, 0) <= 60\
                 && second.groups.reduce((n, g) => n + g.items.length, 0) <= 60\
             })",
        ),
        r#"{"first":[["Today",40,0,40],["Yesterday",50,0,20]],"second":[["Yesterday",50,20,30],["Earlier",70,0,30]],"bounded":true}"#
    );
}

#[test]
fn filter_or_data_identity_changes_reset_the_page() {
    let mut context = context();
    assert_eq!(
        eval(
            &mut context,
            "let state = GalleryWindowCore.updateState(\
               GalleryWindowCore.initialState(),\
               { identity: 'local|all|a,b,c', total: 500 }\
             );\
             state = GalleryWindowCore.setPage(state, 7, 500);\
             const same = GalleryWindowCore.updateState(\
               state,\
               { identity: 'local|all|a,b,c', total: 470 }\
             );\
             const filtered = GalleryWindowCore.updateState(\
               same,\
               { identity: 'local|marked|a,c', total: 120 }\
             );\
             const changedData = GalleryWindowCore.updateState(\
               GalleryWindowCore.setPage(filtered, 1, 120),\
               { identity: 'local|marked|a,c,new', total: 121 }\
             );\
             JSON.stringify({ same: same.page, filtered: filtered.page, changedData: changedData.page })",
        ),
        r#"{"same":7,"filtered":0,"changedData":0}"#
    );
}

#[test]
fn empty_and_out_of_range_pages_are_safe() {
    let mut context = context();
    assert_eq!(
        eval(
            &mut context,
            "let state = GalleryWindowCore.updateState(\
               GalleryWindowCore.initialState(),\
               { identity: 'empty', total: 0 }\
             );\
             const empty = GalleryWindowCore.windowItems([], state);\
             state = GalleryWindowCore.setPage(state, 999, 61);\
             const last = GalleryWindowCore.windowItems(\
               Array.from({ length: 61 }, (_, i) => i),\
               state\
             );\
             JSON.stringify({\
               empty: [empty.page, empty.pageCount, empty.start, empty.end, empty.items.length],\
               last: [last.page, last.pageCount, last.start, last.end, last.items.length, last.hasNext]\
             })",
        ),
        r#"{"empty":[0,0,0,0,0],"last":[1,2,60,61,1,false]}"#
    );
}

#[test]
fn poster_cache_is_lru_bounded_including_unavailable_entries() {
    let mut context = context();
    assert_eq!(
        eval(
            &mut context,
            "const cache = new Map();\
             for (let i = 0; i < 500; i += 1) {\
               GalleryWindowCore.cacheSet(\
                 cache,\
                 `poster-${i}`,\
                 i % 2 ? `asset://${i}` : 'unavailable',\
                 120\
               );\
             }\
             const touched = GalleryWindowCore.cacheGet(cache, 'poster-380');\
             const evicted = GalleryWindowCore.cacheSet(cache, 'poster-500', 'asset://500', 120);\
             JSON.stringify({\
               size: cache.size,\
               oldestGone: !cache.has('poster-0'),\
               nextEvicted: !cache.has('poster-381'),\
               touchedKept: cache.has('poster-380'),\
               touched,\
               evicted\
             })",
        ),
        r#"{"size":120,"oldestGone":true,"nextEvicted":true,"touchedKept":true,"touched":"unavailable","evicted":["poster-381"]}"#
    );
}

#[test]
fn only_a_missing_ffmpeg_runtime_requests_the_global_poster_warning() {
    let mut context = context();
    assert_eq!(
        eval(
            &mut context,
            r#"JSON.stringify([
              "ffmpeg is not available for poster extraction",
              new Error("FFMPEG IS NOT AVAILABLE FOR POSTER EXTRACTION"),
              "ffmpeg poster failed: clip named ffmpeg is not available for poster extraction",
              "ffmpeg poster failed: corrupt input",
              "spawn ffmpeg poster: access denied",
              null
            ].map((error) => GalleryWindowCore.posterRuntimeUnavailable(error)))"#,
        ),
        "[true,true,false,false,false,false]"
    );
}
