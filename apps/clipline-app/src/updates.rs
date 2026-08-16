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
}
