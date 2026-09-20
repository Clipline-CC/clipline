use boa_engine::{Context, Source};

fn context() -> Context {
    let mut ctx = Context::default();
    ctx.eval(Source::from_bytes(
        "function $(id) { return {addEventListener() {}}; } function defaultAudioTrackIds(clip) { return clip && clip.markers && Array.isArray(clip.markers.selected_audio_track_ids) ? clip.markers.selected_audio_track_ids : []; }",
    ))
    .unwrap();
    for source in [
        include_str!("../ui/gallery-window-core.js"),
        include_str!("../ui/library.js"),
        include_str!("../ui/library-cards.js"),
        include_str!("../ui/library-gallery.js"),
        include_str!("../ui/library-rails.js"),
        include_str!("../ui/review-clips.js"),
    ] {
        ctx.eval(Source::from_bytes(source)).unwrap();
    }
    ctx.eval(Source::from_bytes(
        r#"
        var clipsCache = [{path:'C:/a.mp4',group:{name:'G',order:0}}];
        var activeGroupName = 'G';
        function clipKind(c) { return c.kind; }
        var artifact = {path:'C:/out.mp4',kind:'compilation',source_group:'G',
            source_group_fingerprint:groupFingerprint(activeGroup())};
        clipsCache.push(artifact);
    "#,
    ))
    .unwrap();
    ctx
}

#[test]
fn split_gallery_sorts_and_paginates_groups_with_clips() {
    let mut ctx = context();
    ctx.eval(Source::from_bytes(
        r#"
        var galleryFilter = 'all', galleryGameType = 'all', gallerySearch = '';
        var gallerySort = 'marks', galleryGroup = 'game';
        function clipMarkers(c) { return c.markers || []; }
        clipsCache = [
            {path:'C:/a.mp4',group:{name:'G',order:0},game:{name:'Game'},session:'S',
                modified_unix:10,size_mb:10,markers:[1,2]},
            {path:'C:/b.mp4',group:{name:'G',order:1},game:{name:'Game'},session:'S',
                modified_unix:11,size_mb:20,markers:[3]},
            {path:'C:/plain.mp4',game:{name:'Game'},session:'S',
                modified_unix:20,size_mb:1,markers:[1]}
        ];
        var items = [...filterGalleryClips(topLevelLocalClips()).items, ...visibleLocalGroups()];
        var buckets = galleryGroups(sortGalleryClips(items));
        var page = GalleryWindowCore.windowGroups(buckets, GalleryWindowCore.initialState(1));
        var group = page.groups[0].items[0];
        if (page.total !== 2 || page.pageCount !== 2 || group.name !== 'G'
            || group.members.length !== 2 || group.size_mb !== 30 || group.session !== 'S') {
            throw 'split gallery lost integrated group sorting or pagination';
        }
    "#,
    ))
    .unwrap();
}

#[test]
fn only_the_current_compilation_is_hidden() {
    let mut ctx = context();
    for script in [
        "if (topLevelLocalClips().length !== 0) throw 'current artifact visible';",
        "var duplicate = {...artifact,path:'C:/duplicate.mp4'}; clipsCache.push(duplicate);",
        "if (!topLevelLocalClips().includes(duplicate)) throw 'duplicate hidden';",
        "var legacy = {...artifact,path:'C:/legacy.mp4',source_group_fingerprint:null}; clipsCache.push(legacy);",
        "if (!topLevelLocalClips().includes(legacy)) throw 'legacy artifact hidden';",
        "clipsCache.push({path:'C:/b.mp4',group:{name:'G',order:1}});",
        "if (!topLevelLocalClips().includes(artifact)) throw 'stale artifact hidden';",
        "clipsCache = [artifact]; if (!topLevelLocalClips().includes(artifact)) throw 'orphan hidden';",
        "if (!topLevelLocalClips([artifact]).includes(artifact)) throw 'explicit snapshot ignored';",
    ] {
        ctx.eval(Source::from_bytes(script)).unwrap();
    }
}

#[test]
fn reordering_back_does_not_reuse_a_deleted_compilation() {
    let mut ctx = context();
    ctx.eval(Source::from_bytes(
        r#"
        clipsCache.unshift({path:'C:/b.mp4',group:{name:'G',order:1}});
        artifact.source_group_fingerprint = groupFingerprint(activeGroup());
        var PlayerCore = {sameClipPath: (a,b) => a === b};
        var groupReorderPending = false;
        var localClipsRequestGate = {invalidate() {}};
        function setDeckStatus() {}
        function renderClips() {}
        function renderGroupClipRail() {}
        function syncGroupReviewHeader() {}
        function preloadNextGroupMember() {}
        async function invoke(command, args) {
            return args.orderedPaths.map((path,order) => ({path,order}));
        }
        var completed = false;
        (async () => {
            await reorderGroupMembers(activeGroup(), ['C:/b.mp4','C:/a.mp4']);
            await reorderGroupMembers(activeGroup(), ['C:/a.mp4','C:/b.mp4']);
            completed = groupCompilationClip() === null;
        })();
    "#,
    ))
    .unwrap();
    ctx.run_jobs().unwrap();
    assert!(ctx
        .eval(Source::from_bytes("completed"))
        .unwrap()
        .to_boolean());
}
