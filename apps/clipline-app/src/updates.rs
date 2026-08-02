pub use clipline_updater::manifest::UpdateChannel;

pub const fn channel_enabled(channel: UpdateChannel) -> bool {
    channel.enabled()
}

#[cfg(test)]
fn normalize_channel(channel: UpdateChannel) -> UpdateChannel {
    if channel_enabled(channel) {
        channel
    } else {
        UpdateChannel::Nightly
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_channel_is_modeled_but_disabled_for_now() {
        assert!(!channel_enabled(UpdateChannel::Stable));
        assert_eq!(
            normalize_channel(UpdateChannel::Stable),
            UpdateChannel::Nightly
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
