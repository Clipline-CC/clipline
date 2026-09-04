//! Replay-cache run naming and owner identity parsing.

pub const REPLAY_CACHE_RUN_PREFIX: &str = "clipline-replay-cache-";
pub const REPLAY_CACHE_OWNER_FILE: &str = ".clipline-run.json";

pub fn replay_cache_run_identity(name: &str) -> Option<(u128, u32)> {
    let suffix = name.strip_prefix(REPLAY_CACHE_RUN_PREFIX)?;
    let mut parts = suffix.split('-');
    let created_at = parts.next()?;
    let pid = parts.next()?;
    let attempt = parts.next()?;
    if parts.next().is_some()
        || [created_at, pid, attempt]
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return None;
    }
    let created_at = created_at.parse().ok()?;
    let pid = pid.parse().ok()?;
    attempt.parse::<u32>().ok()?;
    Some((created_at, pid))
}

pub fn is_replay_cache_run_name(name: &str) -> bool {
    replay_cache_run_identity(name).is_some()
}

pub fn replay_cache_owner_identity(process_instance_id: &str) -> Option<(u32, u64)> {
    let (pid, creation_time) = process_instance_id.split_once(':')?;
    if [pid, creation_time]
        .iter()
        .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return None;
    }
    Some((pid.parse().ok()?, creation_time.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_cache_run_names_require_three_numeric_components() {
        assert!(is_replay_cache_run_name("clipline-replay-cache-123-456-0"));
        assert_eq!(
            replay_cache_run_identity("clipline-replay-cache-123-456-0"),
            Some((123, 456))
        );
        assert!(!is_replay_cache_run_name("clipline-replay-cache-backup"));
        assert!(!is_replay_cache_run_name(
            "clipline-replay-cache-123-456-0-extra"
        ));
        assert_eq!(replay_cache_owner_identity("456:789"), Some((456, 789)));
        assert_eq!(replay_cache_owner_identity("456:not-a-time"), None);
        assert_eq!(
            replay_cache_owner_identity("456:18446744073709551616"),
            None
        );
    }
}
