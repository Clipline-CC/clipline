use super::*;

/// Only two heavyweight ffmpeg poster children may exist at once.
const MAX_CONCURRENT_POSTER_EXTRACTIONS: usize = 2;

type PosterExtractionResult = Result<PathBuf, String>;

pub(crate) struct PosterExtractionFlight {
    pub(crate) result: tokio::sync::watch::Sender<Option<PosterExtractionResult>>,
}

pub(crate) struct PosterExtractionCoordinator {
    pub(crate) permits: Arc<tokio::sync::Semaphore>,
    pub(crate) flights: tokio::sync::Mutex<HashMap<PathBuf, Arc<PosterExtractionFlight>>>,
}

impl PosterExtractionCoordinator {
    fn new(max_concurrent: usize) -> Self {
        assert!(
            max_concurrent > 0,
            "poster extraction concurrency must be non-zero"
        );
        Self {
            permits: Arc::new(tokio::sync::Semaphore::new(max_concurrent)),
            flights: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Run one blocking extraction per canonical clip path. The worker is
    /// detached from any individual command future so a cancelled caller
    /// cannot strand followers or release its concurrency permit while ffmpeg
    /// is still alive.
    async fn run(
        self: &Arc<Self>,
        canonical_clip: PathBuf,
        work: impl FnOnce() -> PosterExtractionResult + Send + 'static,
    ) -> PosterExtractionResult {
        let (mut result, leader) = {
            let mut flights = self.flights.lock().await;
            if let Some(flight) = flights.get(&canonical_clip) {
                (flight.result.subscribe(), None)
            } else {
                let (result_tx, result_rx) = tokio::sync::watch::channel(None);
                let flight = Arc::new(PosterExtractionFlight { result: result_tx });
                flights.insert(canonical_clip.clone(), Arc::clone(&flight));
                (result_rx, Some(flight))
            }
        };

        if let Some(flight) = leader {
            let coordinator = Arc::clone(self);
            tauri::async_runtime::spawn(async move {
                let completed = coordinator.run_worker(work).await;
                flight.result.send_replace(Some(completed));

                let mut flights = coordinator.flights.lock().await;
                if flights
                    .get(&canonical_clip)
                    .is_some_and(|current| Arc::ptr_eq(current, &flight))
                {
                    flights.remove(&canonical_clip);
                }
            });
        }

        loop {
            if let Some(completed) = result.borrow().clone() {
                return completed;
            }
            result
                .changed()
                .await
                .map_err(|_| "poster extraction ended without a result".to_string())?;
        }
    }

    async fn run_worker(
        &self,
        work: impl FnOnce() -> PosterExtractionResult + Send + 'static,
    ) -> PosterExtractionResult {
        let _permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .map_err(|_| "poster extraction coordinator closed".to_string())?;
        tauri::async_runtime::spawn_blocking(work)
            .await
            .map_err(|error| format!("clip poster task: {error}"))?
    }

    #[cfg(test)]
    async fn joined_callers(&self, canonical_clip: &Path) -> usize {
        self.flights
            .lock()
            .await
            .get(canonical_clip)
            .map_or(0, |flight| flight.result.receiver_count())
    }
}

pub(crate) fn poster_extraction_coordinator() -> Arc<PosterExtractionCoordinator> {
    static COORDINATOR: OnceLock<Arc<PosterExtractionCoordinator>> = OnceLock::new();
    Arc::clone(COORDINATOR.get_or_init(|| {
        Arc::new(PosterExtractionCoordinator::new(
            MAX_CONCURRENT_POSTER_EXTRACTIONS,
        ))
    }))
}

pub(crate) fn poster_failure_kind(error: &str) -> &'static str {
    if error
        .trim()
        .eq_ignore_ascii_case("ffmpeg is not available for poster extraction")
    {
        "runtime_unavailable"
    } else if error.starts_with("spawn ffmpeg poster") {
        "spawn_failed"
    } else if error.contains("timed out") {
        "timeout"
    } else if error.starts_with("ffmpeg poster failed") {
        "media_or_codec"
    } else if error.contains("JPEG data") || error.contains("output limit") {
        "invalid_output"
    } else if error.contains("poster temp") || error.contains("finalize poster") {
        "publish_failed"
    } else {
        "unknown"
    }
}

pub(crate) fn log_poster_failure_once(error: &str) {
    static REPORTED: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let kind = poster_failure_kind(error);
    let reported = REPORTED.get_or_init(|| Mutex::new(HashSet::new()));
    let should_report = match reported.lock() {
        Ok(mut reported) => reported.insert(kind),
        Err(_) => true,
    };
    if should_report {
        // Keep clip paths and FFmpeg stderr out of the support log. The
        // category is enough to distinguish discovery, execution, decoding,
        // output, and Rust-owned publication failures.
        tracing::warn!(event = "poster_extraction_failed", kind);
    }
}

/// Return (generating on demand) the cached poster JPEG for a clip, as a path
/// the webview loads through the asset protocol. Lazy and per-clip so the
/// library listing never blocks on ffmpeg.
#[tauri::command]
pub async fn clip_poster<R: Runtime>(
    app: AppHandle<R>,
    path: String,
    settings: tauri::State<'_, StorageSettings>,
) -> Result<String, String> {
    let scope_root = settings.clips_dir()?;
    let target = validate_clip_path(&settings, &path)?;
    let poster = if let Some(poster) = crate::poster::cached_poster(&target) {
        poster
    } else {
        let canonical_clip = target.clone();
        poster_extraction_coordinator()
            .run(canonical_clip, move || {
                let seek_s = poster_seek_seconds(&target);
                crate::poster::ensure_poster(&target, seek_s)
            })
            .await
            .inspect_err(|error| log_poster_failure_once(error))?
    };
    allow_local_poster_asset(&app, &scope_root, &poster)?;
    Ok(poster.display().to_string())
}

pub(crate) fn allow_local_clip_asset<R: Runtime>(
    app: &AppHandle<R>,
    root: &Path,
    clip: &Path,
) -> Result<(), String> {
    allow_local_media_asset(app, root, clip, &["mp4"])
}

pub(crate) fn allow_local_clip_asset_from_canonical_root<R: Runtime>(
    app: &AppHandle<R>,
    canonical_root: &Path,
    clip: &Path,
) -> Result<(), String> {
    allow_local_media_asset_from_canonical_root(app, canonical_root, clip, &["mp4"])
}

pub(crate) fn allow_local_poster_asset<R: Runtime>(
    app: &AppHandle<R>,
    root: &Path,
    poster: &Path,
) -> Result<(), String> {
    allow_local_media_asset(app, root, poster, &["jpg", "jpeg"])
}

pub(crate) fn allow_local_media_asset<R: Runtime>(
    app: &AppHandle<R>,
    root: &Path,
    asset: &Path,
    extensions: &[&str],
) -> Result<(), String> {
    let canonical_root = canonical_media_root(root)?;
    allow_local_media_asset_from_canonical_root(app, &canonical_root, asset, extensions)
}

pub(crate) fn canonical_media_root(root: &Path) -> Result<PathBuf, String> {
    root.canonicalize()
        .map_err(|e| format!("canonicalize media root {root:?}: {e}"))
}

pub(crate) fn allow_local_media_asset_from_canonical_root<R: Runtime>(
    app: &AppHandle<R>,
    canonical_root: &Path,
    asset: &Path,
    extensions: &[&str],
) -> Result<(), String> {
    let canonical_asset = asset
        .canonicalize()
        .map_err(|e| format!("canonicalize media asset {asset:?}: {e}"))?;
    if !canonical_asset.starts_with(canonical_root) {
        return Err(format!(
            "media asset {canonical_asset:?} escaped root {canonical_root:?}"
        ));
    }
    let extension = canonical_asset
        .extension()
        .and_then(|extension| extension.to_str())
        .ok_or_else(|| format!("media asset {canonical_asset:?} has no extension"))?;
    if !extensions
        .iter()
        .any(|allowed| extension.eq_ignore_ascii_case(allowed))
    {
        return Err(format!(
            "media asset {canonical_asset:?} has an unsupported extension"
        ));
    }
    app.asset_protocol_scope()
        .allow_file(&canonical_asset)
        .map_err(|e| format!("scope media asset {canonical_asset:?} for playback: {e}"))
}

/// The frame to grab a poster from: prefer a local-player review event, then
/// the first review event, else a little into the clip to skip black opening.
/// The frame to grab a poster from: prefer a local-player review event, then
/// the first review event, else a little into the clip to skip black opening.
pub(crate) fn poster_seek_seconds(clip: &Path) -> f64 {
    let Some(markers) = util::read_markers_raw(clip) else {
        return 1.0;
    };
    let markers = filter_review_markers(markers);
    let duration_ok = markers.duration_s.is_finite() && markers.duration_s > 0.0;
    if let Some(first) = markers
        .markers
        .iter()
        .find(|marker| marker.event.involves_local_player)
        .or_else(|| markers.markers.first())
    {
        let t = first.t_s.max(0.0);
        return if duration_ok {
            t.min((markers.duration_s - 0.2).max(0.0))
        } else {
            t
        };
    }
    if duration_ok {
        (markers.duration_s * 0.15).min(5.0)
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_test_utils::TestDir;
    use clipline_events::EventKind;
        #[test]
        fn poster_diagnostics_classify_failures_without_retaining_paths_or_stderr() {
            assert_eq!(
                poster_failure_kind("ffmpeg is not available for poster extraction"),
                "runtime_unavailable"
            );
            assert_eq!(
                poster_failure_kind("spawn ffmpeg poster: access denied"),
                "spawn_failed"
            );
            assert_eq!(
                poster_failure_kind("ffmpeg poster timed out after 30 seconds"),
                "timeout"
            );
            assert_eq!(
                poster_failure_kind("ffmpeg poster failed: C:\\private\\clip.mp4 is corrupt"),
                "media_or_codec"
            );
            assert_eq!(
                poster_failure_kind(
                    "ffmpeg poster failed: clip named ffmpeg is not available for poster extraction"
                ),
                "media_or_codec"
            );
            assert_eq!(
                poster_failure_kind("ffmpeg poster produced no JPEG data"),
                "invalid_output"
            );
            assert_eq!(
                poster_failure_kind("finalize poster: sharing violation"),
                "publish_failed"
            );
        }
        #[test]
        fn poster_seek_seconds_prefers_local_player_marker_for_thumbnail() {
            let dir = TestDir::new("clipline-library", "poster-local-marker");
            let clip = dir.path().join("clip.mp4");
            touch_mp4(&clip);
            let markers = ClipMarkers {
                bookmarks: Vec::new(),
                recording_start_s: 0.0,
                duration_s: 20.0,
                player_summary: None,
                audio_tracks: Vec::new(),
                plays: Vec::new(),
                markers: vec![
                    marker_with(1.0, EventKind::DragonKill, false),
                    marker_with(8.0, EventKind::ChampionAssist, true),
                ],
            };
            std::fs::write(
                clip.with_extension("markers.json"),
                serde_json::to_string(&markers).unwrap(),
            )
            .unwrap();

            assert_eq!(poster_seek_seconds(&clip), 8.0);
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn poster_extraction_is_single_flight_per_canonical_path() {
            use std::sync::atomic::{AtomicUsize, Ordering};
            use std::sync::Arc;

            let coordinator = Arc::new(PosterExtractionCoordinator::new(
                MAX_CONCURRENT_POSTER_EXTRACTIONS,
            ));
            let dir = TestDir::new("clipline-library", "poster-single-flight");
            let clip = dir.path().join("clip.mp4");
            touch_mp4(&clip);
            let key = clip.canonicalize().unwrap();
            let expected_poster = crate::poster::poster_path(&key);
            let calls = Arc::new(AtomicUsize::new(0));
            let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));

            let leader = {
                let coordinator = Arc::clone(&coordinator);
                let key = key.clone();
                let expected_poster = expected_poster.clone();
                let calls = Arc::clone(&calls);
                let release = Arc::clone(&release);
                tokio::spawn(async move {
                    coordinator
                        .run(key, move || {
                            calls.fetch_add(1, Ordering::SeqCst);
                            let (lock, ready) = &*release;
                            let mut released = lock.lock().expect("release lock");
                            while !*released {
                                released = ready.wait(released).expect("release wait");
                            }
                            Ok(expected_poster)
                        })
                        .await
                })
            };

            let leader_started = tokio::time::timeout(Duration::from_secs(2), async {
                while calls.load(Ordering::SeqCst) != 1 {
                    tokio::task::yield_now().await;
                }
            })
            .await;
            if leader_started.is_err() {
                let (lock, ready) = &*release;
                *lock.lock().expect("release lock") = true;
                ready.notify_all();
            }
            leader_started.expect("leader starts");

            let follower = {
                let coordinator = Arc::clone(&coordinator);
                let key = key.clone();
                let calls = Arc::clone(&calls);
                tokio::spawn(async move {
                    coordinator
                        .run(key, move || {
                            calls.fetch_add(1, Ordering::SeqCst);
                            Ok(PathBuf::from("unexpected-second-poster.jpg"))
                        })
                        .await
                })
            };

            let joined = tokio::time::timeout(Duration::from_secs(2), async {
                while coordinator.joined_callers(&key).await != 2 {
                    tokio::task::yield_now().await;
                }
            })
            .await;

            let (lock, ready) = &*release;
            *lock.lock().expect("release lock") = true;
            ready.notify_all();
            joined.expect("follower joins the in-flight extraction");

            let leader_result = leader.await.unwrap().unwrap();
            let follower_result = follower.await.unwrap().unwrap();
            assert_eq!(leader_result, follower_result);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 6)]
        async fn poster_extraction_runs_at_most_two_unique_paths_concurrently() {
            use std::sync::atomic::{AtomicUsize, Ordering};
            use std::sync::Arc;

            let coordinator = Arc::new(PosterExtractionCoordinator::new(
                MAX_CONCURRENT_POSTER_EXTRACTIONS,
            ));
            let active = Arc::new(AtomicUsize::new(0));
            let peak = Arc::new(AtomicUsize::new(0));
            let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
            let mut tasks = Vec::new();

            for index in 0..6 {
                let coordinator = Arc::clone(&coordinator);
                let active = Arc::clone(&active);
                let peak = Arc::clone(&peak);
                let release = Arc::clone(&release);
                tasks.push(tokio::spawn(async move {
                    coordinator
                        .run(
                            PathBuf::from(format!(r"C:\clips\clip-{index}.mp4")),
                            move || {
                                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                                peak.fetch_max(now, Ordering::SeqCst);
                                let (lock, ready) = &*release;
                                let mut released = lock.lock().expect("release lock");
                                while !*released {
                                    released = ready.wait(released).expect("release wait");
                                }
                                active.fetch_sub(1, Ordering::SeqCst);
                                Ok(PathBuf::from(format!(r"C:\clips\clip-{index}.poster.jpg")))
                            },
                        )
                        .await
                }));
            }

            let started = tokio::time::timeout(Duration::from_secs(2), async {
                while active.load(Ordering::SeqCst) != MAX_CONCURRENT_POSTER_EXTRACTIONS {
                    tokio::task::yield_now().await;
                }
            })
            .await;

            let (lock, ready) = &*release;
            *lock.lock().expect("release lock") = true;
            ready.notify_all();
            started.expect("two poster jobs start");
            assert_eq!(peak.load(Ordering::SeqCst), 2);
            for task in tasks {
                task.await.unwrap().unwrap();
            }
            assert_eq!(peak.load(Ordering::SeqCst), 2);
        }
}
