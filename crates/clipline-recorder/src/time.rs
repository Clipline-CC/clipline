//! Shared recorder wall-clock conversion helpers.

use std::time::{SystemTime, UNIX_EPOCH};

#[must_use]
pub fn unix_now() -> u64 {
    unix_seconds_at(SystemTime::now())
}

#[must_use]
pub fn unix_now_i64() -> i64 {
    unix_seconds_i64_at(SystemTime::now())
}

fn unix_seconds_at(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn unix_seconds_i64_at(time: SystemTime) -> i64 {
    i64::try_from(unix_seconds_at(time)).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_unix_seconds_clamps_pre_epoch_and_maps_normal_time() {
        assert_eq!(
            unix_seconds_i64_at(UNIX_EPOCH - std::time::Duration::from_secs(1)),
            0
        );
        assert_eq!(
            unix_seconds_i64_at(UNIX_EPOCH + std::time::Duration::from_secs(42)),
            42
        );
    }
}
