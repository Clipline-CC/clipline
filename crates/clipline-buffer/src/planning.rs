use std::collections::VecDeque;
use std::sync::Arc;

use crate::{DiskSegment, Segment};

pub(crate) trait ReplayWindowSegment {
    fn starts_with_keyframe(&self) -> bool;
    fn pts_start_s(&self) -> f64;
    fn pts_end_s(&self) -> f64;
    fn byte_len(&self) -> usize;
}

impl<T: ReplayWindowSegment> ReplayWindowSegment for Arc<T> {
    fn starts_with_keyframe(&self) -> bool {
        self.as_ref().starts_with_keyframe()
    }

    fn pts_start_s(&self) -> f64 {
        self.as_ref().pts_start_s()
    }

    fn pts_end_s(&self) -> f64 {
        self.as_ref().pts_end_s()
    }

    fn byte_len(&self) -> usize {
        self.as_ref().byte_len()
    }
}

impl ReplayWindowSegment for Segment {
    fn starts_with_keyframe(&self) -> bool {
        self.starts_with_keyframe
    }

    fn pts_start_s(&self) -> f64 {
        self.pts_start_s
    }

    fn pts_end_s(&self) -> f64 {
        self.pts_end_s()
    }

    fn byte_len(&self) -> usize {
        self.byte_len()
    }
}

impl ReplayWindowSegment for DiskSegment {
    fn starts_with_keyframe(&self) -> bool {
        self.starts_with_keyframe
    }

    fn pts_start_s(&self) -> f64 {
        self.pts_start_s
    }

    fn pts_end_s(&self) -> f64 {
        self.pts_end_s()
    }

    fn byte_len(&self) -> usize {
        self.byte_len()
    }
}

pub(crate) fn replay_window_start_index<T: ReplayWindowSegment>(
    segments: &VecDeque<T>,
    window_s: f64,
    exclude_before_s: Option<f64>,
) -> Option<usize> {
    let last = segments.back()?;
    let mut start_target = last.pts_end_s() - window_s;
    if let Some(exclude) = exclude_before_s {
        start_target = start_target.max(exclude);
    }

    let mut start_idx = segments
        .iter()
        .enumerate()
        .filter(|(_, segment)| {
            segment.starts_with_keyframe() && segment.pts_start_s() <= start_target
        })
        .map(|(index, _)| index)
        .next_back()
        .or_else(|| {
            segments
                .iter()
                .position(ReplayWindowSegment::starts_with_keyframe)
        })?;

    if let Some(exclude) = exclude_before_s {
        while start_idx < segments.len() && segments[start_idx].pts_end_s() <= exclude {
            start_idx += 1;
        }
        while start_idx < segments.len() && !segments[start_idx].starts_with_keyframe() {
            start_idx += 1;
        }
    }
    (start_idx < segments.len()).then_some(start_idx)
}

/// How many front segments to drop when a segment ending at
/// `incoming_pts_end_s` is pushed (ddoc §6).
///
/// The byte budget and the retention window are planned **together**, never
/// applied as two passes: whichever demands more eviction wins, and that single
/// count is then advanced to the next keyframe so the surviving front always
/// starts a decodable GOP. Applying them separately would let the byte bound —
/// which removes the minimum count with no keyframe awareness — strand a
/// headless GOP continuation at the front.
///
/// Realignment alone never evicts: with no byte or duration pressure the count
/// stays zero even if the front is a continuation. Where nothing at or after
/// the count starts a keyframe, the count stands rather than draining the ring,
/// leaving `replay_window_start_index`'s first-keyframe fallback to cope.
///
/// Only *existing* segments are counted, so the incoming segment always
/// survives. Under overshoot large enough that the byte budget demands more
/// than retention, the byte bound wins and the retained span falls below
/// `retention_s` — it has to, or the ring grows without bound.
pub(crate) fn eviction_plan<T: ReplayWindowSegment>(
    segments: &VecDeque<T>,
    current_bytes: usize,
    incoming_bytes: usize,
    incoming_pts_end_s: f64,
    max_bytes: usize,
    retention_s: f64,
) -> usize {
    let by_bytes = eviction_count(
        segments.iter().map(ReplayWindowSegment::byte_len),
        current_bytes,
        incoming_bytes,
        max_bytes,
    );
    let by_duration = segments
        .iter()
        .take_while(|segment| incoming_pts_end_s - segment.pts_end_s() > retention_s)
        .count();

    let count = by_bytes.max(by_duration);
    if count == 0 {
        return 0;
    }
    (count..segments.len())
        .find(|index| segments[*index].starts_with_keyframe())
        .unwrap_or(count)
}

pub(crate) fn eviction_count(
    existing_sizes: impl IntoIterator<Item = usize>,
    current_bytes: usize,
    incoming_bytes: usize,
    max_bytes: usize,
) -> usize {
    let mut committed = current_bytes.saturating_add(incoming_bytes);
    let mut count = 0;
    for size in existing_sizes {
        if committed <= max_bytes {
            break;
        }
        committed = committed.saturating_sub(size);
        count += 1;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eviction_never_discards_the_incoming_segment() {
        assert_eq!(eviction_count([], 0, 100, 10), 0);
        assert_eq!(eviction_count([40, 40], 80, 100, 10), 2);
        assert_eq!(eviction_count([40, 40], 80, 10, 50), 1);
    }

    struct Probe {
        key: bool,
        start: f64,
        dur: f64,
        bytes: usize,
    }

    impl ReplayWindowSegment for Probe {
        fn starts_with_keyframe(&self) -> bool {
            self.key
        }

        fn pts_start_s(&self) -> f64 {
            self.start
        }

        fn pts_end_s(&self) -> f64 {
            self.start + self.dur
        }

        fn byte_len(&self) -> usize {
            self.bytes
        }
    }

    /// Back-to-back segments of `dur` seconds and `bytes` apiece, with one
    /// keyframe flag per segment.
    fn gops(dur: f64, bytes: usize, keys: &[bool]) -> VecDeque<Probe> {
        keys.iter()
            .enumerate()
            .map(|(i, key)| Probe {
                key: *key,
                start: i as f64 * dur,
                dur,
                bytes,
            })
            .collect()
    }

    /// Presentation end of the segment arriving right after `segments`.
    fn incoming_end(segments: &VecDeque<Probe>, dur: f64) -> f64 {
        segments.back().map_or(dur, |s| s.pts_end_s() + dur)
    }

    #[test]
    fn byte_pressure_alone_reproduces_the_byte_only_counts() {
        let segments = gops(1.0, 40, &[true, true]);
        let end = incoming_end(&segments, 1.0);

        assert_eq!(eviction_plan(&segments, 80, 100, end, 10, f64::INFINITY), 2);
        assert_eq!(eviction_plan(&segments, 80, 10, end, 50, f64::INFINITY), 1);
        assert_eq!(
            eviction_plan(&VecDeque::<Probe>::new(), 0, 100, 1.0, 10, f64::INFINITY),
            0
        );
    }

    #[test]
    fn duration_pressure_drops_segments_beyond_the_retention_window() {
        let segments = gops(1.0, 10, &[true; 6]);
        let end = incoming_end(&segments, 1.0);

        // Ends run 1..=6, incoming ends at 7.0, so a 3s window keeps ends
        // 4..=6 and drops the three older segments.
        assert_eq!(eviction_plan(&segments, 60, 10, end, usize::MAX, 3.0), 3);
    }

    #[test]
    fn the_larger_of_the_byte_and_duration_counts_wins() {
        let segments = gops(1.0, 10, &[true; 6]);
        let end = incoming_end(&segments, 1.0);

        // Byte budget slack, duration wants three.
        assert_eq!(eviction_plan(&segments, 60, 10, end, usize::MAX, 3.0), 3);
        // Byte budget wants five, duration still wants three.
        assert_eq!(eviction_plan(&segments, 60, 10, end, 20, 3.0), 5);
    }

    #[test]
    fn eviction_advances_to_the_next_keyframe_so_the_front_starts_a_gop() {
        let segments = gops(1.0, 10, &[true, false, false, true, false, false]);
        let end = incoming_end(&segments, 1.0);

        // Duration alone wants one, but index 1 continues the first GOP —
        // advance to the keyframe at index 3 rather than strand a headless GOP.
        assert_eq!(eviction_plan(&segments, 60, 10, end, usize::MAX, 5.0), 3);
    }

    #[test]
    fn no_pressure_never_evicts_even_when_the_front_is_a_continuation() {
        let segments = gops(1.0, 10, &[false, false, true]);
        let end = incoming_end(&segments, 1.0);

        assert_eq!(
            eviction_plan(&segments, 30, 10, end, usize::MAX, f64::INFINITY),
            0,
            "realignment must never be a reason to evict on its own"
        );
    }

    #[test]
    fn plan_may_evict_every_existing_segment_but_never_the_incoming_one() {
        let segments = gops(1.0, 40, &[true, true]);
        let end = incoming_end(&segments, 1.0);

        // Incoming alone busts the cap: both existing go, incoming survives.
        assert_eq!(eviction_plan(&segments, 80, 500, end, 10, f64::INFINITY), 2);
        // Same outcome through the duration bound.
        assert_eq!(eviction_plan(&segments, 80, 40, end, usize::MAX, 0.0), 2);
    }

    #[test]
    fn a_missing_later_keyframe_does_not_over_evict() {
        let segments = gops(1.0, 10, &[true, false, false, false]);
        let end = incoming_end(&segments, 1.0);

        // Nothing at or after index 1 starts a keyframe. Keep the duration
        // count and leave `replay_window_start_index`'s first-keyframe
        // fallback to cope, rather than draining the ring to realign.
        assert_eq!(eviction_plan(&segments, 40, 10, end, usize::MAX, 3.0), 1);
    }

    #[test]
    fn the_byte_bound_wins_when_overshoot_exceeds_the_retention_plan() {
        // Documented envelope limit: bytes must win, or the ring grows without
        // bound. Retention then falls short of the requested window.
        let segments = gops(1.0, 100, &[true; 6]);
        let end = incoming_end(&segments, 1.0);

        let plan = eviction_plan(&segments, 600, 100, end, 200, 5.0);

        assert_eq!(plan, 5, "bytes force eviction past the retention window");
        assert!(
            segments.len() - plan < 5,
            "retained span falls below the retention window under overshoot"
        );
    }
}
