//! Thin compatibility adapter over the framework-neutral poster service.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use clipline_library::PosterService;

fn service() -> &'static PosterService {
    static SERVICE: OnceLock<PosterService> = OnceLock::new();
    SERVICE.get_or_init(PosterService::standard)
}

pub fn ensure_poster(clip: &Path, seek_seconds: f64) -> Result<PathBuf, String> {
    service().ensure_poster(clip, seek_seconds)
}
