use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clipline_library::{
    MAX_CONCURRENT_POSTER_EXTRACTIONS, PosterExtractor, PosterService, cached_poster, poster_path,
};

struct TestDirectory(PathBuf);

fn valid_jpeg() -> Vec<u8> {
    let image = image::RgbImage::from_raw(2, 2, vec![0x33; 12]).unwrap();
    let mut encoded = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image)
        .write_to(&mut encoded, image::ImageFormat::Jpeg)
        .unwrap();
    encoded.into_inner()
}

impl TestDirectory {
    fn new(case: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "clipline-poster-{case}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct CountingExtractor {
    calls: AtomicUsize,
    active: AtomicUsize,
    peak: AtomicUsize,
    delay: Duration,
}

struct FlakyExtractor {
    calls: AtomicUsize,
}

impl PosterExtractor for FlakyExtractor {
    fn extract(&self, clip: &Path, _seek_seconds: f64) -> Result<PathBuf, String> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            return Err("transient extraction failure".to_owned());
        }
        let poster = poster_path(clip);
        std::fs::write(&poster, valid_jpeg()).map_err(|error| error.to_string())?;
        Ok(poster)
    }
}

struct ReplacingExtractor;

impl PosterExtractor for ReplacingExtractor {
    fn extract(&self, clip: &Path, _seek_seconds: f64) -> Result<PathBuf, String> {
        std::fs::remove_file(clip).map_err(|error| error.to_string())?;
        std::fs::write(clip, b"replacement clip").map_err(|error| error.to_string())?;
        let poster = poster_path(clip);
        std::fs::write(&poster, valid_jpeg()).map_err(|error| error.to_string())?;
        Ok(poster)
    }
}

impl CountingExtractor {
    fn new(delay: Duration) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            delay,
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }
}

impl PosterExtractor for CountingExtractor {
    fn extract(&self, clip: &Path, _seek_seconds: f64) -> Result<PathBuf, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        std::thread::sleep(self.delay);
        let poster = poster_path(clip);
        std::fs::write(&poster, valid_jpeg()).map_err(|error| error.to_string())?;
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(poster)
    }
}

#[test]
fn canonical_path_single_flight_runs_one_extractor_and_shares_the_owned_result() {
    let directory = TestDirectory::new("single-flight");
    let clip = directory.0.join("clip.mp4");
    std::fs::write(&clip, b"clip").unwrap();
    let canonical = clip.canonicalize().unwrap();
    let extractor = Arc::new(CountingExtractor::new(Duration::from_millis(40)));
    let service = Arc::new(PosterService::new(extractor.clone()));
    let start = Arc::new(Barrier::new(9));
    let mut callers = Vec::new();
    for _ in 0..8 {
        let service = Arc::clone(&service);
        let canonical = canonical.clone();
        let start = Arc::clone(&start);
        callers.push(std::thread::spawn(move || {
            start.wait();
            service.ensure_poster(&canonical, 1.25)
        }));
    }
    start.wait();
    let results: Vec<_> = callers
        .into_iter()
        .map(|caller| caller.join().unwrap().unwrap())
        .collect();

    assert_eq!(extractor.calls(), 1);
    assert_eq!(service.extraction_starts(), 1);
    assert_eq!(service.single_flight_followers(), 7);
    assert!(results.iter().all(|result| result == &results[0]));
    assert_eq!(std::fs::read(&results[0]).unwrap(), valid_jpeg());
    assert_eq!(cached_poster(&canonical), Some(results[0].clone()));
}

#[test]
fn unique_paths_never_run_more_than_two_extractors_concurrently() {
    let directory = TestDirectory::new("concurrency");
    let extractor = Arc::new(CountingExtractor::new(Duration::from_millis(50)));
    let service = Arc::new(PosterService::new(extractor.clone()));
    let start = Arc::new(Barrier::new(7));
    let mut callers = Vec::new();
    for index in 0..6 {
        let clip = directory.0.join(format!("clip-{index}.mp4"));
        std::fs::write(&clip, b"clip").unwrap();
        let canonical = clip.canonicalize().unwrap();
        let service = Arc::clone(&service);
        let start = Arc::clone(&start);
        callers.push(std::thread::spawn(move || {
            start.wait();
            service.ensure_poster(&canonical, 1.0).unwrap()
        }));
    }
    start.wait();
    for caller in callers {
        caller.join().unwrap();
    }

    assert_eq!(extractor.calls(), 6);
    assert_eq!(service.extraction_starts(), 6);
    assert_eq!(service.single_flight_followers(), 0);
    assert_eq!(extractor.peak(), MAX_CONCURRENT_POSTER_EXTRACTIONS);
    assert_eq!(
        service.peak_active_extractions(),
        MAX_CONCURRENT_POSTER_EXTRACTIONS
    );
}

#[test]
fn cache_hits_bypass_the_extractor_and_replaced_clips_are_stale() {
    let directory = TestDirectory::new("cache-hit");
    let clip = directory.0.join("clip.mp4");
    let poster = poster_path(&clip);
    std::fs::write(&clip, b"clip").unwrap();
    std::fs::write(&poster, valid_jpeg()).unwrap();
    let extractor = Arc::new(CountingExtractor::new(Duration::ZERO));
    let service = PosterService::new(extractor.clone());

    let canonical_poster = poster_path(&clip.canonicalize().unwrap());
    assert_eq!(service.ensure_poster(&clip, 0.0).unwrap(), canonical_poster);
    assert_eq!(extractor.calls(), 0);

    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(&clip, b"replacement clip").unwrap();
    assert_eq!(service.ensure_poster(&clip, 0.0).unwrap(), canonical_poster);
    assert_eq!(extractor.calls(), 1);
}

#[test]
fn failed_flights_are_removed_and_retryable() {
    let directory = TestDirectory::new("retry");
    let clip = directory.0.join("clip.mp4");
    std::fs::write(&clip, b"clip").unwrap();
    let extractor = Arc::new(FlakyExtractor {
        calls: AtomicUsize::new(0),
    });
    let service = PosterService::new(extractor.clone());

    assert!(service.ensure_poster(&clip, 0.0).is_err());
    assert!(service.ensure_poster(&clip, 0.0).is_ok());
    assert_eq!(extractor.calls.load(Ordering::SeqCst), 2);
}

#[test]
fn a_source_replaced_during_extraction_is_rejected() {
    let directory = TestDirectory::new("source-replacement");
    let clip = directory.0.join("clip.mp4");
    std::fs::write(&clip, b"original clip").unwrap();
    let service = PosterService::new(Arc::new(ReplacingExtractor));

    let error = service.ensure_poster(&clip, 0.0).unwrap_err();
    assert!(error.contains("changed during extraction"));
}

#[cfg(any(unix, windows))]
#[test]
fn a_final_symlink_or_reparse_clip_is_rejected_before_extraction() {
    let directory = TestDirectory::new("symlink");
    let real = directory.0.join("real.mp4");
    let link = directory.0.join("link.mp4");
    std::fs::write(&real, b"clip").unwrap();
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(&real, &link);
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_file(&real, &link);
    if linked.is_err() {
        return;
    }
    let extractor = Arc::new(CountingExtractor::new(Duration::ZERO));
    let service = PosterService::new(extractor.clone());

    assert!(service.ensure_poster(&link, 0.0).is_err());
    assert_eq!(extractor.calls(), 0);
}
