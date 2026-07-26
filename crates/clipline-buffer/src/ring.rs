use std::collections::VecDeque;
use std::sync::Arc;

use crate::segment::Segment;

/// Ring of encoded segments bounded by both a byte budget and a retention
/// window (ddoc §6). Eviction is oldest-first and whole-segment, and the
/// eviction count is advanced to the next keyframe so dropping from the front
/// never strands a partial GOP.
///
/// The retention window is what keeps steady-state memory proportional to the
/// footage a save can actually use. The byte budget carries an encoder-overshoot
/// headroom, so on its own it lets the ring grow to the whole budget — roughly
/// twice the intended span at target bitrate, and further still when the encoder
/// undershoots on low-motion content. Under overshoot large enough that the byte
/// budget demands more eviction than retention, the byte bound wins.
#[derive(Debug)]
pub struct ReplayRing {
    max_bytes: usize,
    retention_s: f64,
    segments: VecDeque<Arc<Segment>>,
    bytes: usize,
}

impl ReplayRing {
    /// Byte-budgeted ring with no retention bound. Retention cannot be derived
    /// from a byte budget, so callers that want one must say so explicitly.
    pub fn new(max_bytes: usize) -> Self {
        Self::with_retention(max_bytes, f64::INFINITY)
    }

    /// Ring bounded by a byte budget and a retention window in seconds.
    pub fn with_retention(max_bytes: usize, retention_s: f64) -> Self {
        Self {
            max_bytes,
            retention_s,
            segments: VecDeque::new(),
            bytes: 0,
        }
    }

    pub fn push(&mut self, seg: Segment) {
        self.push_shared(Arc::new(seg));
    }

    /// Insert a segment already shared with another immutable consumer.
    pub fn push_shared(&mut self, seg: Arc<Segment>) {
        let incoming_bytes = seg.byte_len();
        let evict = crate::planning::eviction_plan(
            &self.segments,
            self.bytes,
            incoming_bytes,
            seg.pts_end_s(),
            self.max_bytes,
            self.retention_s,
        );
        for _ in 0..evict {
            if let Some(front) = self.segments.pop_front() {
                self.bytes = self.bytes.saturating_sub(front.byte_len());
            }
        }
        self.bytes = self.bytes.saturating_add(incoming_bytes);
        self.segments.push_back(seg);
    }

    pub fn len(&self) -> usize {
        self.segments.len()
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn segments(&self) -> impl Iterator<Item = &Segment> {
        self.segments.iter().map(Arc::as_ref)
    }

    /// Segments for a Save Replay of the trailing `window_s` seconds
    /// (ddoc §6): starts at the latest keyframe segment at-or-before
    /// `end − window` so the clip decodes cleanly and covers the window.
    ///
    /// `exclude_before_s` is the smart no-overlap mode: footage at or
    /// before that point (the previous save's end) is never re-included.
    pub fn save_window(&self, window_s: f64, exclude_before_s: Option<f64>) -> Vec<&Segment> {
        let Some(idx) =
            crate::planning::replay_window_start_index(&self.segments, window_s, exclude_before_s)
        else {
            return Vec::new();
        };
        self.segments.iter().skip(idx).map(Arc::as_ref).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::Segment;

    fn seg(pts: f64, dur: f64, bytes: usize, key: bool) -> Segment {
        Segment {
            starts_with_keyframe: key,
            pts_start_s: pts,
            duration_s: dur,
            data: vec![0u8; bytes],
            samples: Vec::new(),
            audio: Vec::new(),
        }
    }

    #[test]
    fn evicts_oldest_when_over_byte_budget() {
        let mut ring = ReplayRing::new(250);
        ring.push(seg(0.0, 2.0, 100, true));
        ring.push(seg(2.0, 2.0, 100, true));
        ring.push(seg(4.0, 2.0, 100, true)); // 300 bytes > 250 → evict front
        assert_eq!(ring.len(), 2);
        assert_eq!(ring.bytes(), 200);
        assert_eq!(ring.segments().next().unwrap().pts_start_s, 2.0);
    }

    #[test]
    fn never_evicts_the_only_segment() {
        let mut ring = ReplayRing::new(10);
        ring.push(seg(0.0, 2.0, 100, true)); // oversized but alone
        assert_eq!(ring.len(), 1);
    }

    #[test]
    fn shared_insert_keeps_the_original_segment_allocation() {
        let mut ring = ReplayRing::new(100);
        let shared = Arc::new(seg(0.0, 2.0, 10, true));
        let original = Arc::as_ptr(&shared);

        ring.push_shared(Arc::clone(&shared));

        assert_eq!(ring.segments().next().unwrap() as *const Segment, original);
        assert_eq!(Arc::strong_count(&shared), 2);
    }

    #[test]
    fn eviction_counts_audio_bytes() {
        let mut ring = ReplayRing::new(250);
        let mut s1 = seg(0.0, 2.0, 50, true);
        s1.audio.push(crate::segment::TrackSamples {
            pts_start_s: Some(0.0),
            data: vec![0; 60],
            samples: vec![],
        });
        let mut s2 = seg(2.0, 2.0, 50, true);
        s2.audio.push(crate::segment::TrackSamples {
            pts_start_s: Some(2.0),
            data: vec![0; 60],
            samples: vec![],
        });
        let mut s3 = seg(4.0, 2.0, 50, true);
        s3.audio.push(crate::segment::TrackSamples {
            pts_start_s: Some(4.0),
            data: vec![0; 60],
            samples: vec![],
        });
        ring.push(s1);
        ring.push(s2);
        ring.push(s3); // 330 bytes total > 250 → evict front
        assert_eq!(ring.len(), 2);
        assert_eq!(ring.bytes(), 220);
    }

    #[test]
    fn save_window_starts_at_covering_keyframe() {
        let mut ring = ReplayRing::new(usize::MAX);
        ring.push(seg(0.0, 2.0, 10, true));
        ring.push(seg(2.0, 2.0, 10, true));
        ring.push(seg(4.0, 2.0, 10, true));
        // Window of 3s from end (6.0) → target 3.0, covered by seg@2.0.
        let saved = ring.save_window(3.0, None);
        let starts: Vec<f64> = saved.iter().map(|s| s.pts_start_s).collect();
        assert_eq!(starts, vec![2.0, 4.0]);
    }

    #[test]
    fn save_window_skips_non_keyframe_lead_in() {
        let mut ring = ReplayRing::new(usize::MAX);
        ring.push(seg(0.0, 2.0, 10, true));
        ring.push(seg(2.0, 2.0, 10, false)); // continuation of GOP at 0.0
        ring.push(seg(4.0, 2.0, 10, true));
        // Target 3.0: latest keyframe at/before is 0.0 → include from 0.0
        // so the clip covers the full window and starts decodable.
        let saved = ring.save_window(3.0, None);
        assert_eq!(saved[0].pts_start_s, 0.0);
        assert_eq!(saved.len(), 3);
    }

    #[test]
    fn smart_mode_never_resaves_already_saved_footage() {
        let mut ring = ReplayRing::new(usize::MAX);
        ring.push(seg(0.0, 2.0, 10, true));
        ring.push(seg(2.0, 2.0, 10, true));
        ring.push(seg(4.0, 2.0, 10, true));
        // Previous save consumed up to t=4.0 → only the last segment now.
        let saved = ring.save_window(6.0, Some(4.0));
        let starts: Vec<f64> = saved.iter().map(|s| s.pts_start_s).collect();
        assert_eq!(starts, vec![4.0]);
    }

    #[test]
    fn save_window_on_empty_ring_is_empty() {
        let ring = ReplayRing::new(100);
        assert!(ring.save_window(5.0, None).is_empty());
    }

    /// Feed `total_s` seconds of all-keyframe GOPs into the ring.
    fn steady_state(ring: &mut ReplayRing, gop_s: f64, bytes_per_gop: usize, total_s: f64) {
        let mut pts = 0.0;
        while pts < total_s {
            ring.push(seg(pts, gop_s, bytes_per_gop, true));
            pts += gop_s;
        }
    }

    fn retained_span(ring: &ReplayRing) -> f64 {
        ring.segments().map(|s| s.duration_s).sum()
    }

    #[test]
    fn retention_settles_the_ring_at_the_window_not_the_byte_cap() {
        // Default settings shape: 75s retention against a cap sized with the
        // 2x overshoot headroom (12 Mbps => 214.6 MB), encoder exactly on
        // target at 1.5 MB/s.
        let cap = 214_600_000;
        let mut ring = ReplayRing::with_retention(cap, 75.0);

        steady_state(&mut ring, 0.5, 750_000, 600.0);

        let span = retained_span(&ring);
        assert!(
            (75.0..=76.0).contains(&span),
            "retained {span}s, expected the 75s window"
        );
        assert!(
            ring.bytes() < cap * 55 / 100,
            "retained {} bytes, expected roughly half of the {cap}-byte cap",
            ring.bytes()
        );
    }

    #[test]
    fn retention_holds_when_the_encoder_undershoots_its_target() {
        // The pathological case: at 40% of target bitrate a byte-only ring
        // stretches to ~5x the intended span. Retention must not care.
        let cap = 85_800_000;
        let mut ring = ReplayRing::with_retention(cap, 45.0);

        steady_state(&mut ring, 0.5, 400_000, 600.0);

        let span = retained_span(&ring);
        assert!(
            (45.0..=46.0).contains(&span),
            "retained {span}s, expected the 45s window"
        );
        assert!(ring.bytes() < cap / 2);
    }

    #[test]
    fn byte_accounting_matches_the_segments_retained_after_duration_eviction() {
        let mut ring = ReplayRing::with_retention(usize::MAX, 4.0);

        steady_state(&mut ring, 1.0, 1_000, 60.0);

        let summed: usize = ring.segments().map(Segment::byte_len).sum();
        assert_eq!(ring.bytes(), summed);
        assert_eq!(ring.len(), 5, "4s window plus the segment that closes it");
    }

    #[test]
    fn duration_eviction_keeps_the_front_on_a_gop_boundary() {
        // 2s GOPs sealed as four 0.5s segments: one keyframe, three
        // continuations.
        let mut ring = ReplayRing::with_retention(usize::MAX, 10.0);
        let mut pts = 0.0;
        while pts < 120.0 {
            let key = ((pts / 0.5) as usize).is_multiple_of(4);
            ring.push(seg(pts, 0.5, 1_000, key));
            pts += 0.5;
        }

        assert!(
            ring.segments().next().unwrap().starts_with_keyframe,
            "front must start a GOP after duration eviction"
        );
        assert!(retained_span(&ring) >= 10.0);
    }

    #[test]
    fn save_window_still_covers_the_full_window_after_steady_state() {
        // The guarantee that matters: retention must not eat into the window.
        let mut ring = ReplayRing::with_retention(usize::MAX, 45.0);

        steady_state(&mut ring, 0.5, 100_000, 600.0);

        let saved = ring.save_window(30.0, None);
        let span: f64 = saved.iter().map(|s| s.duration_s).sum();
        assert!(span >= 30.0, "saved only {span}s of a 30s window");
        assert!(saved[0].starts_with_keyframe, "saved clip must decode cleanly");
    }

    #[test]
    fn byte_only_rings_keep_their_unbounded_retention() {
        // `new` stays byte-only so existing callers are unaffected.
        let mut ring = ReplayRing::new(usize::MAX);

        steady_state(&mut ring, 1.0, 1_000, 100.0);

        assert_eq!(ring.len(), 100);
    }
}
