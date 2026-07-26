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

/// How many front segments to drop when `incoming` is pushed (ddoc §6).
///
/// The byte budget and the retention window are planned **together**, never
/// applied as two destructive passes. Duration pressure keeps the latest
/// keyframe at-or-before the requested cutoff: that GOP covers the first frame
/// a replay may save. Only byte pressure can move the start forward, and that
/// later start is aligned to the next available keyframe.
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
    incoming: &T,
    max_bytes: usize,
    retention_s: f64,
) -> usize {
    let by_bytes = eviction_count(
        segments.iter().map(ReplayWindowSegment::byte_len),
        current_bytes,
        incoming.byte_len(),
        max_bytes,
    );

    // The keyframe at-or-before the cutoff is the GOP that covers the first
    // requested frame. Starting at the following keyframe would silently
    // shorten the replay by as much as one GOP.
    let by_duration = if retention_s.is_finite() {
        let cutoff_s = incoming.pts_end_s() - retention_s.max(0.0);
        if incoming.starts_with_keyframe() && incoming.pts_start_s() <= cutoff_s {
            segments.len()
        } else {
            segments
                .iter()
                .enumerate()
                .filter(|(_, segment)| {
                    segment.starts_with_keyframe() && segment.pts_start_s() <= cutoff_s
                })
                .map(|(index, _)| index)
                .next_back()
                .unwrap_or(0)
        }
    } else {
        0
    };

    if by_bytes <= by_duration {
        return by_duration;
    }

    // Only genuine byte pressure may move retention forward. Realign that
    // later start to a keyframe when one is available, including `incoming`.
    (by_bytes..segments.len())
        .find(|index| segments[*index].starts_with_keyframe())
        .or_else(|| incoming.starts_with_keyframe().then_some(segments.len()))
        .unwrap_or(by_bytes)
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

    fn eviction_plan(
        segments: &VecDeque<Probe>,
        current_bytes: usize,
        incoming_bytes: usize,
        incoming_pts_end_s: f64,
        max_bytes: usize,
        retention_s: f64,
    ) -> usize {
        let incoming_start_s = segments.back().map_or(0.0, ReplayWindowSegment::pts_end_s);
        let incoming = Probe {
            key: true,
            start: incoming_start_s,
            dur: incoming_pts_end_s - incoming_start_s,
            bytes: incoming_bytes,
        };
        super::eviction_plan(segments, current_bytes, &incoming, max_bytes, retention_s)
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

        // The incoming segment ends at 7.0, so the exact three-second cutoff
        // is the keyframe at 4.0.
        assert_eq!(eviction_plan(&segments, 60, 10, end, usize::MAX, 3.0), 4);
    }

    #[test]
    fn duration_retention_keeps_the_keyframe_covering_the_cutoff() {
        let segments = gops(1.0, 10, &[true, false, false, true, false, false]);
        let end = incoming_end(&segments, 1.0);

        // The incoming segment ends at 7.0 and the five-second cutoff is 2.0.
        // The GOP beginning at 0.0 is the latest keyframe that covers that
        // cutoff. Advancing to the next keyframe at 3.0 would leave only four
        // seconds for Save Replay.
        assert_eq!(eviction_plan(&segments, 60, 10, end, usize::MAX, 5.0), 0);
    }

    #[test]
    fn exact_duration_boundary_starts_at_the_boundary_keyframe() {
        let segments = gops(1.0, 10, &[true; 6]);
        let end = incoming_end(&segments, 1.0);

        // The retained logical stream ends at 7.0, so an exact three-second
        // window begins at the keyframe at 4.0 (index four).
        assert_eq!(eviction_plan(&segments, 60, 10, end, usize::MAX, 3.0), 4);
    }

    #[test]
    fn the_larger_of_the_byte_and_duration_counts_wins() {
        let segments = gops(1.0, 10, &[true; 6]);
        let end = incoming_end(&segments, 1.0);

        // Byte budget slack, duration wants four.
        assert_eq!(eviction_plan(&segments, 60, 10, end, usize::MAX, 3.0), 4);
        // Byte budget wants five, duration still wants four.
        assert_eq!(eviction_plan(&segments, 60, 10, end, 20, 3.0), 5);
    }

    #[test]
    fn duration_pressure_keeps_the_keyframe_covering_the_cutoff() {
        let segments = gops(1.0, 10, &[true, false, false, true, false, false]);
        let end = incoming_end(&segments, 1.0);

        // The cutoff is 2.0, inside the GOP that starts at zero.
        assert_eq!(eviction_plan(&segments, 60, 10, end, usize::MAX, 5.0), 0);
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

        // The cutoff is 2.0, covered by the GOP at index zero. Do not advance
        // merely because no later existing segment starts a GOP.
        assert_eq!(eviction_plan(&segments, 40, 10, end, usize::MAX, 3.0), 0);
    }

    #[test]
    fn byte_pressure_can_advance_to_the_incoming_keyframe() {
        let segments = gops(1.0, 10, &[true, false, false]);
        let incoming = Probe {
            key: true,
            start: 3.0,
            dur: 1.0,
            bytes: 10,
        };

        assert_eq!(
            super::eviction_plan(&segments, 30, &incoming, 10, 10.0),
            segments.len()
        );
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
