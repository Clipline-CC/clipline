use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub const NIGHTLY_UPDATE_ENDPOINT: &str =
    "https://github.com/dain98/clipline/releases/download/nightly/latest.json";
pub const STABLE_UPDATE_ENDPOINT: &str =
    "https://github.com/dain98/clipline/releases/latest/download/latest.json";

// Standalone builds bundle a fixed WebView2 runtime instead of installing the
// Evergreen runtime system-wide. They must update into the standalone
// installer: the regular one would run the WebView2 bootstrapper on a machine
// whose owner chose not to have WebView2 installed.
pub const NIGHTLY_STANDALONE_UPDATE_ENDPOINT: &str =
    "https://github.com/dain98/clipline/releases/download/nightly/latest-standalone.json";
pub const STABLE_STANDALONE_UPDATE_ENDPOINT: &str =
    "https://github.com/dain98/clipline/releases/latest/download/latest-standalone.json";

/// Official human-readable changelog for both channels. The update dialog
/// links here instead of inlining a truncated notes preview.
pub const CHANGELOG_URL: &str = "https://clipline.cc/changelog";

// Stable GitHub releases publish latest.json as a non-prerelease asset.
pub const STABLE_CHANNEL_ENABLED: bool = true;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpdateChannel {
    Stable,
    Nightly,
}

impl UpdateChannel {
    /// The update channel this install package was built to track. Release
    /// workflows bake `CLIPLINE_DEFAULT_UPDATE_CHANNEL` at build time, so a
    /// Stable download starts on Stable and a Nightly download on Nightly;
    /// local dev builds default to Nightly. A user's saved choice always wins.
    pub fn install_default() -> Self {
        match env!("CLIPLINE_DEFAULT_UPDATE_CHANNEL") {
            "stable" => Self::Stable,
            _ => Self::Nightly,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Stable => "Stable",
            Self::Nightly => "Nightly",
        }
    }

    /// `standalone` is whether this build bundles a fixed WebView2 runtime
    /// (derived from the Tauri config baked into the binary, so an installed
    /// app keeps its variant across updates).
    pub fn endpoint(self, standalone: bool) -> &'static str {
        match (self, standalone) {
            (Self::Stable, false) => STABLE_UPDATE_ENDPOINT,
            (Self::Stable, true) => STABLE_STANDALONE_UPDATE_ENDPOINT,
            (Self::Nightly, false) => NIGHTLY_UPDATE_ENDPOINT,
            (Self::Nightly, true) => NIGHTLY_STANDALONE_UPDATE_ENDPOINT,
        }
    }

    pub fn enabled(self) -> bool {
        match self {
            Self::Stable => STABLE_CHANNEL_ENABLED,
            Self::Nightly => true,
        }
    }
}

impl Default for UpdateChannel {
    fn default() -> Self {
        Self::install_default()
    }
}

pub fn normalize_channel(channel: UpdateChannel) -> UpdateChannel {
    if channel.enabled() {
        channel
    } else {
        UpdateChannel::Nightly
    }
}

/// Marker file asking the next launch to show the window even when argv says
/// autostart. The Windows installer relaunches Clipline with the argv of the
/// process it replaced, so a copy the autostart entry started comes back with
/// `--autostart` and would settle into the tray alone — but whoever pressed
/// Install was looking at the window and expects it back.
pub const UPDATE_RELAUNCH_MARKER_NAME: &str = "pending-update-relaunch";

/// How long a marker stays meaningful. The installer takes over within
/// seconds of writing it; anything older is debris from an install that never
/// finished, and must not surprise some later autostart launch with a window.
pub const UPDATE_RELAUNCH_MAX_AGE: Duration = Duration::from_secs(600);

pub fn update_relaunch_marker_path() -> PathBuf {
    crate::settings::persistence::local_cache_base().join(UPDATE_RELAUNCH_MARKER_NAME)
}

fn unix_secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// Whether a marker stamped at `written_unix_secs` still describes this
/// launch. A clock that moved backwards puts the stamp in the future; count
/// that as fresh rather than dropping a restart the user just asked for.
pub fn update_relaunch_marker_is_fresh(written_unix_secs: u64, now: SystemTime) -> bool {
    unix_secs(now).saturating_sub(written_unix_secs) <= UPDATE_RELAUNCH_MAX_AGE.as_secs()
}

pub fn request_window_on_update_relaunch_at(path: &Path, now: SystemTime) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, unix_secs(now).to_string())
}

pub fn clear_update_relaunch_request_at(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(event = "update_relaunch_marker_clear_failed", error = %error);
        }
    }
}

/// Read the marker and delete it, reporting whether the installer started
/// this launch. Deleting unconditionally is the point: a marker left behind
/// by an install that failed must not reopen the window days later.
pub fn take_update_relaunch_request_at(path: &Path, now: SystemTime) -> bool {
    let recorded = std::fs::read_to_string(path).ok();
    clear_update_relaunch_request_at(path);
    recorded
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .is_some_and(|written| update_relaunch_marker_is_fresh(written, now))
}

pub fn request_window_on_update_relaunch() -> std::io::Result<()> {
    request_window_on_update_relaunch_at(&update_relaunch_marker_path(), SystemTime::now())
}

pub fn clear_update_relaunch_request() {
    clear_update_relaunch_request_at(&update_relaunch_marker_path());
}

pub fn take_update_relaunch_request() -> bool {
    take_update_relaunch_request_at(&update_relaunch_marker_path(), SystemTime::now())
}

/// Whether a launch should settle into the tray without building a window.
/// Autostart launches do — unless the installer started this one, because
/// then a person pressed Install and is waiting for the window to come back.
pub fn should_start_in_tray(launched_by_autostart: bool, relaunched_after_update: bool) -> bool {
    launched_by_autostart && !relaunched_after_update
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_channel_is_enabled_and_kept_on_load() {
        assert!(UpdateChannel::Stable.enabled());
        assert_eq!(
            normalize_channel(UpdateChannel::Stable),
            UpdateChannel::Stable
        );
    }

    #[test]
    fn stable_channel_points_at_github_latest_endpoint() {
        assert_eq!(
            UpdateChannel::Stable.endpoint(false),
            "https://github.com/dain98/clipline/releases/latest/download/latest.json"
        );
    }

    #[test]
    fn nightly_channel_points_at_fixed_github_prerelease_endpoint() {
        assert_eq!(
            UpdateChannel::Nightly.endpoint(false),
            "https://github.com/dain98/clipline/releases/download/nightly/latest.json"
        );
    }

    #[test]
    fn standalone_installs_update_from_the_standalone_manifest() {
        assert_eq!(
            UpdateChannel::Nightly.endpoint(true),
            "https://github.com/dain98/clipline/releases/download/nightly/latest-standalone.json"
        );
        assert_eq!(
            UpdateChannel::Stable.endpoint(true),
            "https://github.com/dain98/clipline/releases/latest/download/latest-standalone.json"
        );
    }

    #[test]
    fn an_update_relaunch_opens_the_window_even_with_autostart_argv() {
        assert!(should_start_in_tray(true, false));
        assert!(!should_start_in_tray(true, true));
        assert!(!should_start_in_tray(false, false));
        assert!(!should_start_in_tray(false, true));
    }

    #[test]
    fn taking_a_fresh_marker_asks_for_the_window_and_consumes_it() {
        let dir = clipline_test_utils::TestDir::new("clipline-updates", "fresh-marker");
        let path = dir.path().join(UPDATE_RELAUNCH_MARKER_NAME);
        let now = SystemTime::now();

        request_window_on_update_relaunch_at(&path, now).unwrap();

        assert!(take_update_relaunch_request_at(&path, now));
        assert!(
            !path.exists(),
            "the marker must not survive the launch it described"
        );
        assert!(!take_update_relaunch_request_at(&path, now));
    }

    #[test]
    fn a_marker_left_by_an_abandoned_install_expires() {
        let dir = clipline_test_utils::TestDir::new("clipline-updates", "stale-marker");
        let path = dir.path().join(UPDATE_RELAUNCH_MARKER_NAME);
        let written = SystemTime::now();
        request_window_on_update_relaunch_at(&path, written).unwrap();

        let much_later = written + UPDATE_RELAUNCH_MAX_AGE + Duration::from_secs(1);
        assert!(!take_update_relaunch_request_at(&path, much_later));
        assert!(!path.exists(), "an expired marker is still cleared");
    }

    #[test]
    fn a_clock_that_moved_backwards_keeps_the_marker_fresh() {
        let written = unix_secs(SystemTime::now());
        let earlier = SystemTime::now() - Duration::from_secs(3600);
        assert!(update_relaunch_marker_is_fresh(written, earlier));
    }

    #[test]
    fn clearing_a_marker_that_was_never_written_is_not_an_error() {
        let dir = clipline_test_utils::TestDir::new("clipline-updates", "missing-marker");
        clear_update_relaunch_request_at(&dir.path().join(UPDATE_RELAUNCH_MARKER_NAME));
    }

    #[test]
    fn a_corrupt_marker_does_not_force_the_window_open() {
        let dir = clipline_test_utils::TestDir::new("clipline-updates", "corrupt-marker");
        let path = dir.path().join(UPDATE_RELAUNCH_MARKER_NAME);
        std::fs::write(&path, "not a timestamp").unwrap();

        assert!(!take_update_relaunch_request_at(&path, SystemTime::now()));
        assert!(!path.exists());
    }
}
