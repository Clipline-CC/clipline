use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use clipline_games::osu_enrichment::{
    apply_scores_to_pending, discover_pending, map_proxy_scores_to_clip_plays, mark_pending_retry,
    pending_path, write_pending_for_saved_clip, JoinedOsuEnrichmentOutcome,
    JoinedOsuEnrichmentService, OsuEnrichmentError, OsuEnrichmentErrorKind, OsuEnrichmentFence,
    OsuEnrichmentPass, OsuEnrichmentRunFuture, OsuEnrichmentService, OsuEnrichmentSummary,
    OsuSavedClip, OsuScoreFetchFuture, OsuScoreFetchPort, SettingsOsuEnrichmentFence,
    MAX_OSU_TITLE_BYTES, MAX_OSU_TITLE_EVENTS,
};
use clipline_games::osu_http::{
    OsuCancellationFuture, OsuHttpError, OsuHttpErrorKind, OsuHttpOwner, OsuProxyScore,
    OsuRecentFetch, OsuRequestFence, OSU_RECENT_SCORE_CEILING,
};
use clipline_library::{
    parse_marker_sidecar_preserving_all, MutationLease, MutationPermit, NoActiveMutationLease,
    OsuEnrichmentStatus, OsuPendingEnrichment, OsuTitleEvent, MAX_CLIP_SIDECAR_PLAYS,
};
use clipline_settings::{
    OsuAccountGeneration, OsuApiSettings, OsuProfileCas, OsuProfileCasKind, SettingsProfile,
    SettingsStore,
};
use clipline_test_utils::TestDir;

use clipline_shell::FileIdentity;

fn owner(value: u64) -> OsuHttpOwner {
    OsuHttpOwner::new(OsuAccountGeneration::new(value).unwrap())
}

#[derive(Default)]
struct Fence {
    owner: Mutex<Option<OsuHttpOwner>>,
    publication: Mutex<()>,
    canceled: AtomicBool,
    notify: tokio::sync::Notify,
}

impl Fence {
    fn new(owner: OsuHttpOwner) -> Self {
        Self {
            owner: Mutex::new(Some(owner)),
            ..Self::default()
        }
    }

    fn replace(&self, owner: OsuHttpOwner) {
        let _publication = self.publication.lock().unwrap();
        *self.owner.lock().unwrap() = Some(owner);
    }

    fn cancel(&self) {
        let _publication = self.publication.lock().unwrap();
        self.canceled.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}

impl OsuRequestFence for Fence {
    fn is_current(&self, owner: OsuHttpOwner) -> bool {
        !self.canceled.load(Ordering::SeqCst) && self.owner.lock().unwrap().as_ref() == Some(&owner)
    }

    fn cancelled<'a>(&'a self, _owner: OsuHttpOwner) -> OsuCancellationFuture<'a> {
        Box::pin(async move {
            if self.canceled.load(Ordering::SeqCst) {
                return;
            }
            self.notify.notified().await;
        })
    }
}

impl OsuEnrichmentFence for Fence {
    fn publish_if_current(
        &self,
        owner: OsuHttpOwner,
        publish: &mut dyn FnMut() -> Result<(), OsuEnrichmentError>,
    ) -> Result<(), OsuEnrichmentError> {
        let _publication = self.publication.lock().unwrap();
        if !self.is_current(owner) {
            return Err(OsuEnrichmentError::account_changed());
        }
        publish()
    }
}

fn write_session(dir: &std::path::Path) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("clipline-session.json"),
        br#"{"id":"osu","name":"osu!"}"#,
    )
    .unwrap();
    let clip = dir.join("session_123.mp4");
    std::fs::write(&clip, b"mp4").unwrap();
    clip
}

fn pending_record(clip: &std::path::Path) -> OsuPendingEnrichment {
    OsuPendingEnrichment {
        schema_version: 1,
        clip_path: clip.display().to_string(),
        recording_start_unix: 1_000,
        recording_end_unix: 2_000,
        clip_duration_s: 1_000.0,
        status: OsuEnrichmentStatus::Pending,
        attempts: 0,
        pagination_ceiling_reached: false,
        title_events: Vec::new(),
        message: None,
    }
}

fn score(id: usize, ended_at: i64) -> OsuProxyScore {
    OsuProxyScore {
        id: id.to_string(),
        url: Some(format!("https://osu.ppy.sh/scores/osu/{id}")),
        beatmap_id: Some(1),
        beatmapset_id: Some(2),
        cover_url: None,
        title: "Blue Zenith".into(),
        artist: "xi".into(),
        difficulty: "FOUR DIMENSIONS".into(),
        mapper: Some("Asphyxia".into()),
        star_rating: Some(7.0),
        mods: vec!["HD".into()],
        rank: Some("S".into()),
        passed: true,
        accuracy: Some(0.99),
        max_combo: Some(700),
        total_score: Some(1_000_000),
        pp: Some(300.0),
        started_at_unix: Some(ended_at - 1),
        ended_at_unix: ended_at,
        beatmap_total_length_s: Some(1.0),
    }
}

#[test]
fn mapping_accepts_exact_http_and_title_compatibility_ceilings() {
    assert_eq!(MAX_CLIP_SIDECAR_PLAYS, MAX_OSU_TITLE_EVENTS);
    let record = OsuPendingEnrichment {
        clip_path: "clip.mp4".into(),
        ..pending_record(std::path::Path::new("clip.mp4"))
    };
    let scores: Vec<_> = (0..OSU_RECENT_SCORE_CEILING)
        .map(|id| score(id, 1_001 + id as i64))
        .collect();

    let mapped = map_proxy_scores_to_clip_plays(&record, &scores, true).unwrap();

    assert_eq!(mapped.plays.len(), OSU_RECENT_SCORE_CEILING);
    assert!(mapped.pagination_ceiling_reached);
}

#[test]
fn saved_clip_writes_512_title_plays_and_shared_parser_accepts_them() {
    let dir = TestDir::new("clipline-osu", "title-512");
    let clip = write_session(&dir.path().join("session"));
    let title_events = (0..MAX_OSU_TITLE_EVENTS)
        .map(|index| OsuTitleEvent {
            unix_s: 1_000 + index as i64,
            title: format!("osu! - Artist - Song {index} [Difficulty]"),
        })
        .collect();

    write_pending_for_saved_clip(
        &OsuSavedClip {
            path: clip.clone(),
            seconds: 600.0,
            full_session: true,
            recording_start_unix: Some(1_000),
            recording_end_unix: Some(1_600),
            title_events,
        },
        &NoActiveMutationLease,
    )
    .unwrap();

    let marker_bytes = std::fs::read(clip.with_extension("markers.json")).unwrap();
    let markers = parse_marker_sidecar_preserving_all(&marker_bytes).unwrap();
    assert_eq!(markers.plays.len(), MAX_OSU_TITLE_EVENTS);
    assert!(pending_path(&clip).exists());
}

#[test]
fn exact_plus_one_title_bounds_fail_before_publication() {
    let dir = TestDir::new("clipline-osu", "title-overflow");
    let clip = write_session(&dir.path().join("session"));
    let mut titles = vec![
        OsuTitleEvent {
            unix_s: 1_000,
            title: "osu! - Artist - Song [Difficulty]".into(),
        };
        MAX_OSU_TITLE_EVENTS + 1
    ];
    titles[0].title = "x".repeat(MAX_OSU_TITLE_BYTES + 1);

    let error = write_pending_for_saved_clip(
        &OsuSavedClip {
            path: clip.clone(),
            seconds: 600.0,
            full_session: true,
            recording_start_unix: Some(1_000),
            recording_end_unix: Some(1_600),
            title_events: titles,
        },
        &NoActiveMutationLease,
    )
    .unwrap_err();

    assert_eq!(error.kind(), OsuEnrichmentErrorKind::TooLarge);
    assert!(!pending_path(&clip).exists());
    assert!(!clip.with_extension("markers.json").exists());
}

#[test]
fn marker_replacement_preserves_non_review_events_and_other_fields() {
    let dir = TestDir::new("clipline-osu", "preserve-markers");
    let clip = write_session(&dir.path().join("session"));
    let existing = serde_json::json!({
        "recording_start_s": 10.0,
        "duration_s": 60.0,
        "audio_tracks": [],
        "plays": [],
        "markers": [{
            "t_s": 0.0,
            "game_id": "LeagueOfLegends",
            "kind": "GameStart",
            "actor": "Dain",
            "victim": "Opponent",
            "assisters": [],
            "subtype": null,
            "game_time_s": 42.0,
            "recording_offset_s": 142.0,
            "importance": 1,
            "involves_local_player": false
        }]
    });
    std::fs::write(
        clip.with_extension("markers.json"),
        serde_json::to_vec_pretty(&existing).unwrap(),
    )
    .unwrap();
    write_pending_for_saved_clip(
        &OsuSavedClip {
            path: clip.clone(),
            seconds: 60.0,
            full_session: true,
            recording_start_unix: Some(1_000),
            recording_end_unix: Some(1_060),
            title_events: vec![OsuTitleEvent {
                unix_s: 1_010,
                title: "osu! - xi - Blue Zenith [FOUR DIMENSIONS]".into(),
            }],
        },
        &NoActiveMutationLease,
    )
    .unwrap();

    let markers = parse_marker_sidecar_preserving_all(
        &std::fs::read(clip.with_extension("markers.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(markers.recording_start_s, 10.0);
    assert_eq!(markers.markers.len(), 1);
    assert_eq!(markers.plays.len(), 1);
}

#[test]
fn foreign_pending_and_clip_replacements_are_preserved() {
    let dir = TestDir::new("clipline-osu", "foreign-races");
    let clip = write_session(&dir.path().join("session"));
    let pending = pending_record(&clip);
    std::fs::write(
        pending_path(&clip),
        serde_json::to_vec_pretty(&pending).unwrap(),
    )
    .unwrap();
    let mut jobs = discover_pending(dir.path()).unwrap();
    let job = jobs.remove(0);
    std::fs::remove_file(pending_path(&clip)).unwrap();
    std::fs::write(pending_path(&clip), b"foreign").unwrap();

    let error = mark_pending_retry(&job, "retry").unwrap_err();
    assert_eq!(error.kind(), OsuEnrichmentErrorKind::StaleFile);
    assert_eq!(std::fs::read(pending_path(&clip)).unwrap(), b"foreign");
    let error = apply_scores_to_pending(&job, &[score(1, 1_020)], false, &NoActiveMutationLease)
        .unwrap_err();
    assert_eq!(error.kind(), OsuEnrichmentErrorKind::StaleFile);
    assert!(!clip.with_extension("markers.json").exists());

    std::fs::remove_file(&clip).unwrap();
    std::fs::write(&clip, b"foreign clip").unwrap();
    let error = apply_scores_to_pending(&job, &[score(1, 1_020)], false, &NoActiveMutationLease)
        .unwrap_err();
    assert_eq!(error.kind(), OsuEnrichmentErrorKind::StaleFile);
    assert_eq!(std::fs::read(&clip).unwrap(), b"foreign clip");
}

#[cfg(unix)]
#[test]
fn discovered_job_publishes_only_through_its_retained_parent_authority() {
    let dir = TestDir::new("clipline-osu", "retained-parent");
    let session = dir.path().join("session");
    let clip = pending_fixture(dir.path());
    let mut jobs = discover_pending(dir.path()).unwrap();
    let job = jobs.remove(0);
    let selected = dir.path().join("selected-session");
    std::fs::rename(&session, &selected).unwrap();
    std::fs::create_dir(&session).unwrap();
    let foreign_pending = pending_path(&clip);
    std::fs::write(&foreign_pending, b"foreign").unwrap();

    mark_pending_retry(&job, "retry selected parent").unwrap();

    assert_eq!(std::fs::read(&foreign_pending).unwrap(), b"foreign");
    let selected_pending = selected.join(
        foreign_pending
            .file_name()
            .expect("pending fixture has a file name"),
    );
    let updated: OsuPendingEnrichment =
        serde_json::from_slice(&std::fs::read(selected_pending).unwrap()).unwrap();
    assert_eq!(updated.attempts, 1);
    assert_eq!(updated.message.as_deref(), Some("retry selected parent"));
}

struct FetchState {
    owner: OsuHttpOwner,
    calls: AtomicUsize,
    wait: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    result: Mutex<Option<OsuRecentFetch>>,
}

#[derive(Clone)]
struct Fetcher(Arc<FetchState>);

impl OsuScoreFetchPort for Fetcher {
    fn owner(&self) -> OsuHttpOwner {
        self.0.owner
    }

    fn fetch<'a>(
        &'a self,
        _stop_before_unix: Option<i64>,
        _fence: &'a dyn OsuRequestFence,
    ) -> OsuScoreFetchFuture<'a> {
        Box::pin(async move {
            self.0.calls.fetch_add(1, Ordering::SeqCst);
            self.0.entered.notify_waiters();
            if self.0.wait.load(Ordering::SeqCst) {
                self.0.release.notified().await;
            }
            Ok(self.0.result.lock().unwrap().take().unwrap())
        })
    }
}

struct FailingFetcher {
    owner: OsuHttpOwner,
}

impl OsuScoreFetchPort for FailingFetcher {
    fn owner(&self) -> OsuHttpOwner {
        self.owner
    }

    fn fetch<'a>(
        &'a self,
        _stop_before_unix: Option<i64>,
        _fence: &'a dyn OsuRequestFence,
    ) -> OsuScoreFetchFuture<'a> {
        Box::pin(async {
            Err(OsuHttpError::new(
                OsuHttpErrorKind::Offline,
                "injected offline osu! API",
            ))
        })
    }
}

fn fetcher(owner: OsuHttpOwner, scores: Vec<OsuProxyScore>, wait: bool) -> Fetcher {
    Fetcher(Arc::new(FetchState {
        owner,
        calls: AtomicUsize::new(0),
        wait: AtomicBool::new(wait),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        result: Mutex::new(Some(OsuRecentFetch {
            owner,
            user_id: "1".into(),
            scores,
            failed_count: 0,
            started_at_count: 1,
            ended_at_count: 1,
            pagination_ceiling_reached: false,
            username: Some("player".into()),
        })),
    }))
}

fn pending_fixture(root: &std::path::Path) -> std::path::PathBuf {
    let clip = write_session(&root.join("session"));
    let pending = pending_record(&clip);
    std::fs::write(
        pending_path(&clip),
        serde_json::to_vec_pretty(&pending).unwrap(),
    )
    .unwrap();
    clip
}

#[tokio::test]
async fn service_updates_once_and_rejects_same_root_overlap() {
    let dir = TestDir::new("clipline-osu", "single-flight");
    let clip = pending_fixture(dir.path());
    let owner = owner(8);
    let fetcher = fetcher(owner, vec![score(1, 1_020)], true);
    let service = Arc::new(OsuEnrichmentService::new(
        fetcher.clone(),
        Arc::new(NoActiveMutationLease),
    ));
    let fence = Arc::new(Fence::new(owner));
    let first = {
        let service = Arc::clone(&service);
        let fence = Arc::clone(&fence);
        let root = dir.path().to_path_buf();
        tokio::spawn(async move { service.run(&root, u64::MAX, fence.as_ref()).await })
    };
    fetcher.0.entered.notified().await;

    let error = service
        .run(dir.path(), u64::MAX, fence.as_ref())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), OsuEnrichmentErrorKind::AlreadyRunning);
    assert_eq!(fetcher.0.calls.load(Ordering::SeqCst), 1);

    fetcher.0.release.notify_waiters();
    let summary = first.await.unwrap().unwrap();
    assert_eq!(summary.updated, 1);
    assert!(!pending_path(&clip).exists());
    assert_eq!(
        parse_marker_sidecar_preserving_all(
            &std::fs::read(clip.with_extension("markers.json")).unwrap()
        )
        .unwrap()
        .plays
        .len(),
        1
    );
}

#[tokio::test]
async fn stale_account_and_cancel_do_not_publish() {
    let dir = TestDir::new("clipline-osu", "stale-account");
    let clip = pending_fixture(dir.path());
    let original_owner = owner(9);
    let fetcher = fetcher(original_owner, vec![score(1, 1_020)], true);
    let service = Arc::new(OsuEnrichmentService::new(
        fetcher.clone(),
        Arc::new(NoActiveMutationLease),
    ));
    let fence = Arc::new(Fence::new(original_owner));
    let task = {
        let service = Arc::clone(&service);
        let fence = Arc::clone(&fence);
        let root = dir.path().to_path_buf();
        tokio::spawn(async move { service.run(&root, u64::MAX, fence.as_ref()).await })
    };
    fetcher.0.entered.notified().await;
    fence.replace(owner(10));
    fetcher.0.release.notify_waiters();

    let error = task.await.unwrap().unwrap_err();
    assert_eq!(error.kind(), OsuEnrichmentErrorKind::AccountChanged);
    assert!(pending_path(&clip).exists());
    assert!(!clip.with_extension("markers.json").exists());

    fence.cancel();
    assert!(!fence.is_current(original_owner));
}

#[derive(Debug)]
struct AlwaysBusyMutationLease;

impl MutationLease for AlwaysBusyMutationLease {
    fn acquire(
        &self,
        _canonical_path: &std::path::Path,
        _identity: FileIdentity,
    ) -> Result<Box<dyn MutationPermit>, String> {
        Err("clip is active".into())
    }
}

#[tokio::test]
async fn active_upload_or_playback_defers_publication_and_keeps_pending_work() {
    let dir = TestDir::new("clipline-osu", "active-mutation");
    let clip = pending_fixture(dir.path());
    let owner = owner(11);
    let fetcher = fetcher(owner, vec![score(1, 1_020)], false);
    let service = OsuEnrichmentService::new(fetcher, Arc::new(AlwaysBusyMutationLease));
    let fence = Fence::new(owner);

    let summary = service.run(dir.path(), u64::MAX, &fence).await.unwrap();

    assert_eq!(summary.attempted, 1);
    assert_eq!(summary.updated, 0);
    assert_eq!(summary.retry_scheduled, 1);
    assert!(pending_path(&clip).exists());
    assert!(!clip.with_extension("markers.json").exists());
}

#[tokio::test]
async fn http_failure_does_not_rewrite_pending_work_while_mutation_is_busy() {
    let dir = TestDir::new("clipline-osu", "busy-http-failure");
    let clip = pending_fixture(dir.path());
    let before = std::fs::read(pending_path(&clip)).unwrap();
    let owner = owner(12);
    let service =
        OsuEnrichmentService::new(FailingFetcher { owner }, Arc::new(AlwaysBusyMutationLease));
    let fence = Fence::new(owner);

    let error = service.run(dir.path(), u64::MAX, &fence).await.unwrap_err();

    assert_eq!(error.kind(), OsuEnrichmentErrorKind::Http);
    assert_eq!(std::fs::read(pending_path(&clip)).unwrap(), before);
    assert!(!clip.with_extension("markers.json").exists());
}

#[test]
fn settings_fence_linearizes_publication_and_rejects_replacement() {
    let dir = TestDir::new("clipline-osu", "settings-publication-fence");
    let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
    let initial = store.snapshot().unwrap();
    let generation = initial
        .document
        .osu
        .account_generation
        .checked_next()
        .unwrap();
    let configured = OsuApiSettings {
        account_generation: generation,
        client_id: Some("12345".into()),
        user: Some("Dain".into()),
        credential_target: Some(
            clipline_settings::osu::osu_credential_target_for_operation(generation, "initial")
                .unwrap(),
        ),
        credential_cleanup_targets: Vec::new(),
        last_connected_username: None,
    };
    let configured = store
        .compare_exchange_osu_profile(OsuProfileCas {
            kind: OsuProfileCasKind::Save,
            expected: initial.document.osu,
            replacement: configured,
        })
        .unwrap()
        .document
        .osu;
    let fence = SettingsOsuEnrichmentFence::new(store.clone(), configured.clone());
    let mut published = false;
    fence
        .publish_if_current(fence.owner(), &mut || {
            published = true;
            Ok(())
        })
        .unwrap();
    assert!(published);

    let next_generation = generation.checked_next().unwrap();
    let mut replacement = OsuApiSettings {
        account_generation: next_generation,
        client_id: Some("12345".into()),
        user: Some("3426414".into()),
        credential_target: Some(
            clipline_settings::osu::osu_credential_target_for_operation(
                next_generation,
                "replacement",
            )
            .unwrap(),
        ),
        credential_cleanup_targets: vec![configured.credential_target.clone().unwrap()],
        last_connected_username: Some("Dain".into()),
    };
    replacement.normalize();
    store
        .compare_exchange_osu_profile(OsuProfileCas {
            kind: OsuProfileCasKind::Test,
            expected: configured,
            replacement,
        })
        .unwrap();
    assert!(!fence.is_current(fence.owner()));
    assert_eq!(
        fence
            .publish_if_current(fence.owner(), &mut || Ok(()))
            .unwrap_err()
            .kind(),
        OsuEnrichmentErrorKind::AccountChanged
    );
}

#[test]
fn settings_fence_cancel_waits_for_publication_and_rejects_later_writes() {
    let dir = TestDir::new("clipline-osu", "settings-cancel-publication");
    let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
    let initial = store.snapshot().unwrap();
    let generation = initial
        .document
        .osu
        .account_generation
        .checked_next()
        .unwrap();
    let configured = OsuApiSettings {
        account_generation: generation,
        client_id: Some("12345".into()),
        user: Some("Dain".into()),
        credential_target: Some(
            clipline_settings::osu::osu_credential_target_for_operation(generation, "cancel")
                .unwrap(),
        ),
        credential_cleanup_targets: Vec::new(),
        last_connected_username: None,
    };
    let configured = store
        .compare_exchange_osu_profile(OsuProfileCas {
            kind: OsuProfileCasKind::Save,
            expected: initial.document.osu,
            replacement: configured,
        })
        .unwrap()
        .document
        .osu;
    let fence = Arc::new(SettingsOsuEnrichmentFence::new(store, configured));
    let (entered_tx, entered_rx) = mpsc::sync_channel(0);
    let (release_tx, release_rx) = mpsc::sync_channel(0);
    let publication = {
        let fence = Arc::clone(&fence);
        std::thread::spawn(move || {
            fence.publish_if_current(fence.owner(), &mut || {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
        })
    };
    entered_rx.recv().unwrap();
    let (canceled_tx, canceled_rx) = mpsc::sync_channel(0);
    let cancellation = {
        let fence = Arc::clone(&fence);
        std::thread::spawn(move || {
            fence.cancel();
            canceled_tx.send(()).unwrap();
        })
    };
    assert!(canceled_rx.recv_timeout(Duration::from_millis(50)).is_err());

    release_tx.send(()).unwrap();
    publication.join().unwrap().unwrap();
    canceled_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    cancellation.join().unwrap();
    assert_eq!(
        fence
            .publish_if_current(fence.owner(), &mut || Ok(()))
            .unwrap_err()
            .kind(),
        OsuEnrichmentErrorKind::AccountChanged
    );
}

struct CoordinatorPass {
    owner: OsuHttpOwner,
    calls: Arc<AtomicUsize>,
    entered: Arc<tokio::sync::Notify>,
    release: Option<Arc<tokio::sync::Notify>>,
    panic_after_release: bool,
}

impl OsuEnrichmentPass for CoordinatorPass {
    fn run<'a>(
        &'a self,
        _media_root: &'a std::path::Path,
        _expected_root: FileIdentity,
        _now_unix: u64,
        _fence: &'a dyn OsuEnrichmentFence,
    ) -> OsuEnrichmentRunFuture<'a> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_one();
            if let Some(release) = &self.release {
                release.notified().await;
            }
            assert!(!self.panic_after_release, "injected enrichment panic");
            Ok(OsuEnrichmentSummary {
                owner: self.owner,
                discovered: 0,
                attempted: 0,
                updated: 0,
                retry_scheduled: 0,
                failed: 0,
                pagination_ceiling_reached: false,
            })
        })
    }
}

#[tokio::test]
async fn joined_service_runs_one_active_and_only_the_latest_coalesced_request() {
    let dir = TestDir::new("clipline-osu", "joined-coalescing");
    let owner = owner(20);
    let fence = Arc::new(Fence::new(owner));
    let service = JoinedOsuEnrichmentService::start().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let first_entered = Arc::new(tokio::sync::Notify::new());
    let first_release = Arc::new(tokio::sync::Notify::new());
    let first = service
        .submit(
            dir.path(),
            1,
            Arc::new(CoordinatorPass {
                owner,
                calls: Arc::clone(&calls),
                entered: Arc::clone(&first_entered),
                release: Some(Arc::clone(&first_release)),
                panic_after_release: false,
            }),
            fence.clone(),
        )
        .unwrap();
    first_entered.notified().await;

    let displaced = service
        .submit(
            dir.path(),
            2,
            Arc::new(CoordinatorPass {
                owner,
                calls: Arc::clone(&calls),
                entered: Arc::new(tokio::sync::Notify::new()),
                release: None,
                panic_after_release: false,
            }),
            fence.clone(),
        )
        .unwrap();
    let latest_entered = Arc::new(tokio::sync::Notify::new());
    let latest = service
        .submit(
            dir.path(),
            3,
            Arc::new(CoordinatorPass {
                owner,
                calls: Arc::clone(&calls),
                entered: Arc::clone(&latest_entered),
                release: None,
                panic_after_release: false,
            }),
            fence,
        )
        .unwrap();
    assert!(matches!(
        displaced
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap(),
        JoinedOsuEnrichmentOutcome::Superseded
    ));

    first_release.notify_one();
    latest_entered.notified().await;
    assert!(matches!(
        first
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap(),
        JoinedOsuEnrichmentOutcome::Completed(_)
    ));
    assert!(matches!(
        latest
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap(),
        JoinedOsuEnrichmentOutcome::Completed(_)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    service.shutdown().unwrap();
}

#[tokio::test]
async fn joined_service_contains_a_pass_panic_and_runs_the_pending_request() {
    let dir = TestDir::new("clipline-osu", "joined-panic");
    let owner = owner(21);
    let fence = Arc::new(Fence::new(owner));
    let service = JoinedOsuEnrichmentService::start().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let first_entered = Arc::new(tokio::sync::Notify::new());
    let first_release = Arc::new(tokio::sync::Notify::new());
    let first = service
        .submit(
            dir.path(),
            1,
            Arc::new(CoordinatorPass {
                owner,
                calls: Arc::clone(&calls),
                entered: Arc::clone(&first_entered),
                release: Some(Arc::clone(&first_release)),
                panic_after_release: true,
            }),
            fence.clone(),
        )
        .unwrap();
    first_entered.notified().await;
    let pending = service
        .submit(
            dir.path(),
            2,
            Arc::new(CoordinatorPass {
                owner,
                calls: Arc::clone(&calls),
                entered: Arc::new(tokio::sync::Notify::new()),
                release: None,
                panic_after_release: false,
            }),
            fence,
        )
        .unwrap();
    first_release.notify_one();

    assert!(matches!(
        first
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap(),
        JoinedOsuEnrichmentOutcome::Panicked
    ));
    assert!(matches!(
        pending
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap(),
        JoinedOsuEnrichmentOutcome::Completed(_)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    service.shutdown().unwrap();
}
