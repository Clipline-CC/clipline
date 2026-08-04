//! PCM continuity between QPC-stamped WASAPI chunks and exact Opus frames.
//! Loopback goes quiet when nothing renders; the MP4 audio timeline is
//! duration-cumulative, so gaps MUST become silence or A/V desyncs.

use std::collections::VecDeque;

#[cfg(any(windows, test))]
use std::time::{Duration, Instant};

#[cfg(any(windows, test))]
use crate::opus::FRAME_DURATION_S;
use crate::opus::FRAME_LEN;

const SAMPLE_RATE: f64 = 48_000.0;
/// Gaps shorter than half a frame are treated as device jitter.
#[cfg(any(windows, test))]
const GAP_TOLERANCE_S: f64 = FRAME_DURATION_S / 2.0;
/// A bogus device timestamp must not allocate unbounded silence.
const MAX_GAP_FILL_S: f64 = 5.0;
const FRAME_PAIRS: u64 = (FRAME_LEN / 2) as u64;
#[cfg(any(windows, test))]
const DISCONTINUITY_FADE_PAIRS: usize = 1_920; // 40 ms at 48 kHz.
const LATE_RECOVERY_FADE_PAIRS: usize = 240; // 5 ms at 48 kHz.
#[cfg(any(windows, test))]
const DEVICE_PACKET_QUIET_GRACE_S: f64 = 0.1;

pub type PcmFrame = (f64, Vec<f32>);

/// Selects how each WASAPI device packet joins the shared recording timeline.
/// QPC establishes the initial anchor and re-anchors after genuine quiet;
/// continuously arriving PCM keeps its authoritative sample-count cadence.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg(any(windows, test))]
pub(crate) enum DevicePacketPlacement {
    Timestamped(f64),
    Contiguous,
}

#[cfg(any(windows, test))]
pub(crate) struct DevicePacketTimeline {
    needs_timestamp_anchor: bool,
}

#[cfg(any(windows, test))]
impl DevicePacketTimeline {
    pub(crate) fn new() -> Self {
        Self {
            needs_timestamp_anchor: true,
        }
    }

    pub(crate) fn placement(&mut self, pts_s: Option<f64>) -> DevicePacketPlacement {
        if !self.needs_timestamp_anchor {
            return DevicePacketPlacement::Contiguous;
        }
        let Some(pts_s) = pts_s.filter(|pts_s| pts_s.is_finite()) else {
            return DevicePacketPlacement::Contiguous;
        };
        self.needs_timestamp_anchor = false;
        DevicePacketPlacement::Timestamped(pts_s)
    }

    pub(crate) fn require_timestamp_anchor(&mut self) {
        self.needs_timestamp_anchor = true;
    }

    /// Packet timestamps from some drivers drift relative to the shared video
    /// clock and cannot identify quiet. Only host-observed delivery idleness
    /// permits the next packet to establish a fresh QPC anchor.
    pub(crate) fn note_synthesized_silence(&mut self, idle_s: f64) -> bool {
        if !idle_s.is_finite() || idle_s + f64::EPSILON < DEVICE_PACKET_QUIET_GRACE_S {
            return false;
        }
        self.needs_timestamp_anchor = true;
        true
    }
}

/// Throttled re-activation state for a lost WASAPI endpoint. The first
/// failure opens the outage; retries are spaced by `retry_interval` while
/// the outage keeps measuring from that first failure so diagnostics can
/// report how long the track has been silent.
#[cfg(any(windows, test))]
pub(crate) struct DeviceReactivation {
    retry_interval: Duration,
    dead_since: Option<Instant>,
    next_retry_at: Option<Instant>,
}

#[cfg(any(windows, test))]
impl DeviceReactivation {
    pub(crate) fn new(retry_interval: Duration) -> Self {
        Self {
            retry_interval,
            dead_since: None,
            next_retry_at: None,
        }
    }

    pub(crate) fn is_live(&self) -> bool {
        self.dead_since.is_none()
    }

    pub(crate) fn note_lost(&mut self, now: Instant) -> bool {
        if !self.is_live() {
            return false;
        }
        self.dead_since = Some(now);
        self.next_retry_at = Some(now + self.retry_interval);
        true
    }

    pub(crate) fn note_retry_failed(&mut self, now: Instant) {
        debug_assert!(!self.is_live(), "cannot fail a retry for a live device");
        self.next_retry_at = Some(now + self.retry_interval);
    }

    pub(crate) fn retry_due(&self, now: Instant) -> bool {
        !self.is_live() && self.next_retry_at.is_some_and(|at| now >= at)
    }

    pub(crate) fn note_recovered(&mut self, now: Instant) -> Option<Duration> {
        let outage = self
            .dead_since
            .map(|since| now.saturating_duration_since(since));
        self.dead_since = None;
        self.next_retry_at = None;
        outage
    }
}

#[cfg(any(windows, test))]
pub(crate) struct DiscontinuityFade {
    remaining_pairs: usize,
}

#[cfg(any(windows, test))]
impl DiscontinuityFade {
    pub(crate) fn new() -> Self {
        Self {
            remaining_pairs: DISCONTINUITY_FADE_PAIRS,
        }
    }

    pub(crate) fn restart(&mut self) {
        self.remaining_pairs = DISCONTINUITY_FADE_PAIRS;
    }

    pub(crate) fn apply(&mut self, interleaved: &mut [f32]) {
        if self.remaining_pairs == 0
            || !interleaved
                .chunks_exact(2)
                .any(|pair| pair[0] != 0.0 || pair[1] != 0.0)
        {
            return;
        }

        let mut fade_started = self.remaining_pairs < DISCONTINUITY_FADE_PAIRS;
        for pair in interleaved.chunks_exact_mut(2) {
            if self.remaining_pairs == 0 {
                break;
            }
            if !fade_started && pair[0] == 0.0 && pair[1] == 0.0 {
                continue;
            }
            fade_started = true;
            let completed = DISCONTINUITY_FADE_PAIRS - self.remaining_pairs;
            let gain = completed as f32 / (DISCONTINUITY_FADE_PAIRS - 1) as f32;
            pair[0] *= gain;
            pair[1] *= gain;
            self.remaining_pairs -= 1;
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[cfg(any(windows, test))]
pub(crate) struct PcmPushOutcome {
    pub late_reanchor_s: Option<f64>,
    pub total_correction_s: f64,
    pub chunk_duration_s: f64,
}

#[derive(Clone, Copy, Debug)]
struct TimelineAnchor {
    pair_index: u64,
    pts_s: f64,
}

#[derive(Default)]
pub struct LoopbackAssembler {
    /// Timestamp anchors at absolute stereo-pair positions. Ordinary input
    /// needs only the initial anchor; a capped clock discontinuity appends a
    /// new one without allocating the entire missing interval as silence.
    anchors: VecDeque<TimelineAnchor>,
    /// Expected WASAPI timestamp for the next finite chunk.
    next_chunk_pts_s: Option<f64>,
    /// Persistent correction established when synthesized silence overtakes
    /// delayed source timestamps. Following chunks keep their cadence rather
    /// than falling behind the synthetic frontier again.
    #[cfg(any(windows, test))]
    source_pts_correction_s: f64,
    synthesized_since_real: bool,
    late_recovery_fade_remaining_pairs: usize,
    buffered: Vec<f32>, // interleaved stereo
    frames_popped: u64,
}

impl LoopbackAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// `pts_s` stamps the first sample of `interleaved` (stereo pairs).
    #[cfg(any(windows, test))]
    pub(crate) fn push_chunk(&mut self, pts_s: f64, interleaved: &[f32]) -> PcmPushOutcome {
        if !pts_s.is_finite() {
            self.push_contiguous_chunk(interleaved);
            return PcmPushOutcome::default();
        }
        let mut corrected_pts_s = pts_s + self.source_pts_correction_s;
        self.ensure_initial_anchor(corrected_pts_s);
        let expected = self.next_chunk_pts_s.unwrap_or(corrected_pts_s);
        let mut gap = corrected_pts_s - expected;
        let correction = if gap < -GAP_TOLERANCE_S && self.synthesized_since_real {
            let correction_s = expected - corrected_pts_s;
            self.source_pts_correction_s += correction_s;
            corrected_pts_s = expected;
            gap = 0.0;
            self.late_recovery_fade_remaining_pairs = LATE_RECOVERY_FADE_PAIRS;
            Some(correction_s)
        } else {
            None
        };
        let mut samples = interleaved;
        if gap > GAP_TOLERANCE_S {
            let mut missing_pairs = (gap.min(MAX_GAP_FILL_S) * SAMPLE_RATE).round() as u64;
            if gap > MAX_GAP_FILL_S {
                // End the bounded silence on a packet boundary so no Opus
                // frame straddles the old and re-anchored clocks.
                let end_remainder = (self.written_pairs() + missing_pairs) % FRAME_PAIRS;
                missing_pairs = missing_pairs.saturating_sub(end_remainder);
            }
            self.buffered
                .extend(std::iter::repeat_n(0.0, missing_pairs as usize * 2));
            if gap > MAX_GAP_FILL_S {
                self.reanchor_next_pair(corrected_pts_s);
            }
        } else if gap < -GAP_TOLERANCE_S {
            let overlap_pairs = ((expected - corrected_pts_s) * SAMPLE_RATE).round() as usize;
            if overlap_pairs >= interleaved.len() / 2 {
                return PcmPushOutcome::default();
            }
            samples = &interleaved[overlap_pairs * 2..];
        }
        self.append_live_samples(samples);
        let chunk_duration_s = (samples.len() / 2) as f64 / SAMPLE_RATE;
        self.next_chunk_pts_s = Some(if gap > GAP_TOLERANCE_S {
            corrected_pts_s + chunk_duration_s
        } else {
            expected + chunk_duration_s
        });
        self.synthesized_since_real = false;
        PcmPushOutcome {
            late_reanchor_s: correction,
            total_correction_s: self.source_pts_correction_s,
            chunk_duration_s,
        }
    }

    /// Extend an anchored timeline with stereo silence up to an absolute PTS.
    /// Non-finite or non-forward targets are ignored. One call is bounded by
    /// the same limit used for timestamp-discovered gaps.
    pub fn advance_with_silence(&mut self, target_pts_s: f64) {
        if !target_pts_s.is_finite() {
            return;
        }
        let Some(expected_pts_s) = self.next_chunk_pts_s else {
            return;
        };
        let gap_s = target_pts_s - expected_pts_s;
        if gap_s <= 0.0 {
            return;
        }
        let missing_pairs = (gap_s.min(MAX_GAP_FILL_S) * SAMPLE_RATE).round() as usize;
        self.buffered
            .extend(std::iter::repeat_n(0.0, missing_pairs * 2));
        self.next_chunk_pts_s = Some(expected_pts_s + missing_pairs as f64 / SAMPLE_RATE);
        self.synthesized_since_real |= missing_pairs > 0;
    }

    /// Append a chunk when the device explicitly marked its timestamp invalid.
    pub fn push_contiguous_chunk(&mut self, interleaved: &[f32]) {
        self.ensure_initial_anchor(0.0);
        self.append_live_samples(interleaved);
        if let Some(next_chunk_pts_s) = &mut self.next_chunk_pts_s {
            *next_chunk_pts_s += (interleaved.len() / 2) as f64 / SAMPLE_RATE;
        }
        self.synthesized_since_real = false;
    }

    /// One 20 ms frame once enough samples are buffered.
    pub fn pop_frame(&mut self) -> Option<PcmFrame> {
        let mut samples = Vec::with_capacity(FRAME_LEN);
        let pts = self.pop_frame_into(&mut samples)?;
        Some((pts, samples))
    }

    /// Append one 20 ms frame into caller-owned storage. The monitor path
    /// reserves its fixed output once and uses this to avoid a per-poll frame
    /// allocation; recording retains the owned-frame wrapper above.
    pub fn pop_frame_into(&mut self, samples: &mut Vec<f32>) -> Option<f64> {
        if self.buffered.len() < FRAME_LEN {
            return None;
        }
        let pair_index = self.frames_popped * FRAME_PAIRS;
        while self
            .anchors
            .get(1)
            .is_some_and(|anchor| anchor.pair_index <= pair_index)
        {
            self.anchors.pop_front();
        }
        let anchor = self.anchors.front()?;
        let pts = anchor.pts_s + (pair_index - anchor.pair_index) as f64 / SAMPLE_RATE;
        samples.extend(self.buffered.drain(..FRAME_LEN));
        self.frames_popped += 1;
        Some(pts)
    }

    fn written_pairs(&self) -> u64 {
        self.frames_popped * FRAME_PAIRS + (self.buffered.len() / 2) as u64
    }

    fn append_live_samples(&mut self, interleaved: &[f32]) {
        let fade_pairs = self
            .late_recovery_fade_remaining_pairs
            .min(interleaved.len() / 2);
        self.buffered.reserve(interleaved.len());
        for pair in interleaved[..fade_pairs * 2].chunks_exact(2) {
            let completed = LATE_RECOVERY_FADE_PAIRS - self.late_recovery_fade_remaining_pairs;
            let gain = completed as f32 / (LATE_RECOVERY_FADE_PAIRS - 1) as f32;
            self.buffered.push(pair[0] * gain);
            self.buffered.push(pair[1] * gain);
            self.late_recovery_fade_remaining_pairs -= 1;
        }
        self.buffered
            .extend_from_slice(&interleaved[fade_pairs * 2..]);
    }

    fn ensure_initial_anchor(&mut self, pts_s: f64) {
        if self.anchors.is_empty() {
            self.anchors.push_back(TimelineAnchor {
                pair_index: self.written_pairs(),
                pts_s,
            });
        }
    }

    #[cfg(any(windows, test))]
    fn reanchor_next_pair(&mut self, source_pts_s: f64) {
        let pair_index = self.written_pairs();
        let prior_grid_pts = self.anchors.back().map_or(source_pts_s, |anchor| {
            anchor.pts_s + (pair_index - anchor.pair_index) as f64 / SAMPLE_RATE
        });
        let pts_s = source_pts_s.max(prior_grid_pts);
        if let Some(last) = self
            .anchors
            .back_mut()
            .filter(|a| a.pair_index == pair_index)
        {
            last.pts_s = last.pts_s.max(pts_s);
        } else {
            self.anchors.push_back(TimelineAnchor { pair_index, pts_s });
        }
    }
}

/// First two channels of N-channel interleaved float PCM (mono duplicates).
pub fn extract_stereo(samples: &[f32], channels: u16) -> Vec<f32> {
    let ch = channels.max(1) as usize;
    let mut out = Vec::with_capacity(samples.len() / ch * 2);
    extract_stereo_into(samples, channels, &mut out);
    out
}

/// Extract stereo into caller-owned storage so real-time monitor paths can
/// reserve once at endpoint activation.
pub fn extract_stereo_into(samples: &[f32], channels: u16, out: &mut Vec<f32>) {
    out.clear();
    let ch = channels.max(1) as usize;
    for frame in samples.chunks_exact(ch) {
        out.push(frame[0]);
        out.push(if ch >= 2 { frame[1] } else { frame[0] });
    }
}

/// Average every source channel into a centered stereo pair. A stereo mic
/// that only uses one side becomes centered at half amplitude by design.
pub fn extract_mono_centered(samples: &[f32], channels: u16) -> Vec<f32> {
    let ch = channels.max(1) as usize;
    let mut out = Vec::with_capacity(samples.len() / ch * 2);
    extract_mono_centered_into(samples, channels, &mut out);
    out
}

/// Center all source channels into caller-owned stereo storage.
pub fn extract_mono_centered_into(samples: &[f32], channels: u16, out: &mut Vec<f32>) {
    out.clear();
    let ch = channels.max(1) as usize;
    for frame in samples.chunks_exact(ch) {
        let mono = frame.iter().sum::<f32>() / ch as f32;
        out.push(mono);
        out.push(mono);
    }
}

pub fn apply_gain(samples: &mut [f32], gain: f32) {
    for sample in samples {
        *sample = (*sample * gain).clamp(-1.0, 1.0);
    }
}

#[derive(Debug)]
pub struct StereoResampler {
    from_rate: u32,
    to_rate: u32,
    next_source_frame: f64,
    input_frames_seen: u64,
    last_frame: Option<[f32; 2]>,
}

impl StereoResampler {
    pub fn new(from_rate: u32, to_rate: u32) -> Self {
        Self {
            from_rate,
            to_rate,
            next_source_frame: 0.0,
            input_frames_seen: 0,
            last_frame: None,
        }
    }

    pub fn resample(&mut self, samples: &[f32]) -> Vec<f32> {
        let mut out = Vec::new();
        self.resample_into(samples, &mut out);
        out
    }

    /// Stateful conversion into caller-owned storage. Callers that reserve
    /// [`maximum_resampled_stereo_samples`] once do not allocate per chunk.
    pub fn resample_into(&mut self, samples: &[f32], out: &mut Vec<f32>) {
        out.clear();
        if samples.is_empty() || self.from_rate == 0 || self.to_rate == 0 {
            return;
        }
        let in_frames = samples.len() / 2;
        if in_frames == 0 {
            return;
        }
        if self.from_rate == self.to_rate {
            self.input_frames_seen += in_frames as u64;
            self.next_source_frame = self.input_frames_seen as f64;
            self.last_frame = Some(last_stereo_frame(samples, in_frames));
            out.extend_from_slice(&samples[..in_frames * 2]);
            return;
        }

        let chunk_start = self.input_frames_seen as f64;
        let chunk_end = chunk_start + in_frames as f64;
        let step = self.from_rate as f64 / self.to_rate as f64;
        while self.next_source_frame < chunk_end - 1e-12 {
            let relative_pos = self.next_source_frame - chunk_start;
            let [left, right] = self.sample_at(samples, in_frames, relative_pos);
            out.push(left);
            out.push(right);
            self.next_source_frame += step;
        }

        self.input_frames_seen += in_frames as u64;
        self.last_frame = Some(last_stereo_frame(samples, in_frames));
    }

    fn sample_at(&self, samples: &[f32], in_frames: usize, relative_pos: f64) -> [f32; 2] {
        if relative_pos < 0.0 {
            let current = [samples[0], samples[1]];
            let previous = self.last_frame.unwrap_or(current);
            let t = (relative_pos + 1.0).clamp(0.0, 1.0) as f32;
            return [
                previous[0] + (current[0] - previous[0]) * t,
                previous[1] + (current[1] - previous[1]) * t,
            ];
        }

        let a = relative_pos.floor() as usize;
        if a + 1 >= in_frames {
            return last_stereo_frame(samples, in_frames);
        }
        let b = a + 1;
        let t = (relative_pos - a as f64) as f32;
        [
            samples[a * 2] + (samples[b * 2] - samples[a * 2]) * t,
            samples[a * 2 + 1] + (samples[b * 2 + 1] - samples[a * 2 + 1]) * t,
        ]
    }
}

/// Conservative output bound for one stereo resampler call, including the
/// one-frame interpolation carry at each chunk boundary.
#[must_use]
pub fn maximum_resampled_stereo_samples(
    input_frames: usize,
    from_rate: u32,
    to_rate: u32,
) -> Option<usize> {
    if from_rate == 0 || to_rate == 0 {
        return Some(0);
    }
    let frames = input_frames
        .checked_mul(to_rate as usize)?
        .checked_add(from_rate as usize - 1)?
        .checked_div(from_rate as usize)?
        .checked_add(1)?;
    frames.checked_mul(2)
}

fn last_stereo_frame(samples: &[f32], in_frames: usize) -> [f32; 2] {
    [
        samples[(in_frames - 1) * 2],
        samples[(in_frames - 1) * 2 + 1],
    ]
}

pub fn resample_stereo_linear(samples: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    StereoResampler::new(from_rate, to_rate).resample(samples)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opus::{FRAME_DURATION_S, FRAME_LEN};

    fn pairs(n: usize, v: f32) -> Vec<f32> {
        vec![v; n * 2]
    }

    #[test]
    fn slices_contiguous_chunks_into_frames() {
        let mut asm = LoopbackAssembler::new();
        asm.push_chunk(1.0, &pairs(960, 0.5)); // exactly one frame
        asm.push_chunk(1.02, &pairs(480, 0.25)); // half a frame
        let (pts, frame) = asm.pop_frame().expect("one full frame");
        assert_eq!(pts, 1.0);
        assert_eq!(frame.len(), FRAME_LEN);
        assert!(frame.iter().all(|&s| s == 0.5));
        assert!(asm.pop_frame().is_none(), "half frame still pending");
        asm.push_chunk(1.03, &pairs(480, 0.25));
        let (pts2, _) = asm.pop_frame().expect("second frame");
        assert!((pts2 - (1.0 + FRAME_DURATION_S)).abs() < 1e-9);
    }

    #[test]
    fn fills_gaps_with_silence() {
        let mut asm = LoopbackAssembler::new();
        asm.push_chunk(0.0, &pairs(960, 1.0));
        // 40 ms gap (nothing rendered): next chunk stamps at 0.06.
        asm.push_chunk(0.06, &pairs(960, 1.0));
        let mut frames = Vec::new();
        while let Some(f) = asm.pop_frame() {
            frames.push(f);
        }
        assert_eq!(frames.len(), 4, "audio + 2 silence frames + audio");
        assert!(frames[0].1.iter().all(|&s| s == 1.0));
        assert!(frames[1].1.iter().all(|&s| s == 0.0), "gap became silence");
        assert!(frames[2].1.iter().all(|&s| s == 0.0), "gap became silence");
        assert!(frames[3].1.iter().all(|&s| s == 1.0), "audio resumes");
        // pts stays continuous despite the gap: the resumed audio lands at 0.06.
        assert!((frames[3].0 - 0.06).abs() < 1e-9);
    }

    #[test]
    fn anchored_assembler_advances_through_device_silence() {
        let mut asm = LoopbackAssembler::new();
        asm.push_chunk(0.0, &[]);

        asm.advance_with_silence(0.5);

        let mut frames = Vec::new();
        while let Some(frame) = asm.pop_frame() {
            frames.push(frame);
        }
        assert_eq!(frames.len(), 25);
        for (index, (pts_s, frame)) in frames.iter().enumerate() {
            assert!((*pts_s - index as f64 * FRAME_DURATION_S).abs() < 1e-9);
            assert!(frame.iter().all(|&sample| sample == 0.0));
        }
    }

    #[test]
    fn silence_advancement_is_monotonic_and_idempotent() {
        let mut asm = LoopbackAssembler::new();
        asm.push_chunk(0.0, &[]);

        asm.advance_with_silence(0.04);
        asm.advance_with_silence(0.02);
        asm.advance_with_silence(0.04);

        assert_eq!(std::iter::from_fn(|| asm.pop_frame()).count(), 2);
    }

    #[test]
    fn non_finite_silence_horizon_does_not_advance() {
        let mut asm = LoopbackAssembler::new();
        asm.push_chunk(0.0, &[]);

        asm.advance_with_silence(f64::INFINITY);
        asm.advance_with_silence(f64::NAN);

        assert!(asm.pop_frame().is_none());
    }

    #[test]
    fn caps_huge_timestamp_gaps() {
        let mut asm = LoopbackAssembler::new();
        asm.push_chunk(0.0, &pairs(960, 1.0));
        asm.push_chunk(3600.0, &pairs(960, 1.0));

        let max_missing_pairs = (MAX_GAP_FILL_S * SAMPLE_RATE).round() as usize;
        assert_eq!(asm.buffered.len(), (960 + max_missing_pairs + 960) * 2);
    }

    #[test]
    fn capped_timestamp_gap_does_not_repeat_for_following_chunks() {
        let mut asm = LoopbackAssembler::new();
        asm.push_chunk(0.0, &pairs(960, 1.0));
        asm.push_chunk(3600.0, &pairs(960, 0.5));
        asm.push_chunk(3600.02, &pairs(960, 0.25));

        let max_missing_pairs = (MAX_GAP_FILL_S * SAMPLE_RATE).round() as usize;
        assert_eq!(
            asm.buffered.len(),
            (960 + max_missing_pairs + 960 + 960) * 2
        );
        let frames: Vec<_> = std::iter::from_fn(|| asm.pop_frame()).collect();
        assert!((frames[frames.len() - 2].0 - 3600.0).abs() < 1e-9);
        assert!((frames[frames.len() - 1].0 - 3600.02).abs() < 1e-9);
    }

    #[test]
    fn discontinuity_anchor_starts_on_a_complete_frame_after_partial_pcm() {
        let mut asm = LoopbackAssembler::new();
        asm.push_chunk(0.0, &pairs(480, 1.0));
        asm.push_chunk(3600.0, &pairs(960, 0.5));

        let frames: Vec<_> = std::iter::from_fn(|| asm.pop_frame()).collect();
        let resumed = frames.last().expect("resumed source frame");
        assert!((resumed.0 - 3600.0).abs() < 1e-9);
        assert!(resumed.1.iter().all(|&sample| sample == 0.5));
    }

    #[test]
    fn invalid_timestamp_chunks_append_contiguously() {
        let mut asm = LoopbackAssembler::new();
        asm.push_chunk(0.0, &pairs(960, 1.0));
        asm.push_chunk(f64::NAN, &pairs(960, 0.5));

        let first = asm.pop_frame().expect("first frame");
        let second = asm.pop_frame().expect("second frame");
        assert!(first.1.iter().all(|&s| s == 1.0));
        assert!(second.1.iter().all(|&s| s == 0.5));
        assert!(asm.pop_frame().is_none());
    }

    #[test]
    fn small_jitter_does_not_insert_silence() {
        let mut asm = LoopbackAssembler::new();
        asm.push_chunk(0.0, &pairs(960, 1.0));
        // 5 ms late — within tolerance, treated as contiguous.
        asm.push_chunk(0.025, &pairs(960, 1.0));
        let mut n = 0;
        while asm.pop_frame().is_some() {
            n += 1;
        }
        assert_eq!(n, 2, "no silence frames inserted");
    }

    #[test]
    fn partially_overlapped_live_chunk_is_reanchored_without_stuttering() {
        let mut asm = LoopbackAssembler::new();
        asm.push_chunk(0.0, &[]);
        asm.advance_with_silence(0.10);

        let outcome = asm.push_chunk(0.08, &pairs(1_920, 0.75));

        let mut frames = Vec::new();
        while let Some(frame) = asm.pop_frame() {
            frames.push(frame);
        }
        assert!((outcome.late_reanchor_s.unwrap() - 0.02).abs() < 1e-9);
        assert!((outcome.total_correction_s - 0.02).abs() < 1e-9);
        assert!((outcome.chunk_duration_s - 0.04).abs() < 1e-9);
        assert_eq!(frames.len(), 7);
        assert!(frames[..5]
            .iter()
            .all(|(_, frame)| frame.iter().all(|&sample| sample == 0.0)));
        assert!((frames[5].0 - 0.10).abs() < 1e-9);
        assert_eq!(&frames[5].1[..2], &[0.0, 0.0]);
        assert!(frames[5].1[480..].iter().all(|&sample| sample == 0.75));
        assert!((frames[6].0 - 0.12).abs() < 1e-9);
        assert!(frames[6].1.iter().all(|&sample| sample == 0.75));
    }

    #[test]
    fn late_live_recovery_fades_in_without_dropping_samples() {
        let mut asm = LoopbackAssembler::new();
        asm.push_chunk(0.0, &[]);
        asm.advance_with_silence(0.10);

        let outcome = asm.push_chunk(0.08, &pairs(960, 1.0));
        let frames: Vec<_> = std::iter::from_fn(|| asm.pop_frame()).collect();

        assert!(outcome.late_reanchor_s.is_some());
        assert_eq!(frames.len(), 6, "no live sample may be discarded");
        let recovered = &frames[5].1;
        assert_eq!(&recovered[..2], &[0.0, 0.0]);
        assert!(recovered[240] > 0.49 && recovered[240] < 0.52);
        assert_eq!(&recovered[478..482], &[1.0, 1.0, 1.0, 1.0]);
        assert!(recovered[482..].iter().all(|&sample| sample == 1.0));
    }

    #[test]
    fn synthesized_silence_does_not_permanently_discard_late_live_audio() {
        let mut asm = LoopbackAssembler::new();
        asm.push_chunk(0.0, &[]);
        asm.advance_with_silence(0.10);
        assert_eq!(
            std::iter::from_fn(|| asm.pop_frame()).count(),
            5,
            "silence has already been committed before the source catches up"
        );

        let first = asm.push_chunk(0.04, &pairs(960, 0.75));
        let second = asm.push_chunk(0.06, &pairs(960, 0.50));

        let resumed: Vec<_> = std::iter::from_fn(|| asm.pop_frame()).collect();
        assert!((first.late_reanchor_s.unwrap() - 0.06).abs() < 1e-9);
        assert_eq!(second.late_reanchor_s, None);
        assert_eq!(
            resumed.len(),
            2,
            "late audio must resume instead of locking out"
        );
        assert!((resumed[0].0 - 0.10).abs() < 1e-9);
        assert_eq!(&resumed[0].1[..2], &[0.0, 0.0]);
        assert!(resumed[0].1[480..].iter().all(|&sample| sample == 0.75));
        assert!((resumed[1].0 - 0.12).abs() < 1e-9);
        assert!(resumed[1].1.iter().all(|&sample| sample == 0.50));
    }

    #[test]
    fn duplicate_late_chunk_without_synthetic_advance_is_still_discarded() {
        let mut asm = LoopbackAssembler::new();
        asm.push_chunk(0.0, &pairs(960, 0.75));

        let duplicate = asm.push_chunk(0.0, &pairs(960, 0.50));

        assert_eq!(duplicate.late_reanchor_s, None);
        let frames: Vec<_> = std::iter::from_fn(|| asm.pop_frame()).collect();
        assert_eq!(frames.len(), 1);
        assert!(frames[0].1.iter().all(|&sample| sample == 0.75));
    }

    #[test]
    fn seeded_assemblers_share_a_frame_grid_despite_phase_offsets() {
        let mut a = LoopbackAssembler::new();
        let mut b = LoopbackAssembler::new();
        a.push_chunk(0.0, &[]);
        b.push_chunk(0.0, &[]);

        a.push_chunk(0.004, &pairs(960, 1.0));
        b.push_chunk(0.012, &pairs(960, 1.0));

        let (a_pts, _) = a.pop_frame().expect("source a frame");
        let (b_pts, _) = b.pop_frame().expect("source b frame");
        assert_eq!(a_pts, 0.0);
        assert_eq!(b_pts, 0.0);
    }

    #[test]
    fn extract_stereo_handles_channel_counts() {
        // Stereo passes through.
        assert_eq!(
            extract_stereo(&[1.0, 2.0, 3.0, 4.0], 2),
            vec![1.0, 2.0, 3.0, 4.0]
        );
        // 5.1 keeps front L/R.
        let six: Vec<f32> = (0..12).map(|i| i as f32).collect();
        assert_eq!(extract_stereo(&six, 6), vec![0.0, 1.0, 6.0, 7.0]);
        // Mono duplicates.
        assert_eq!(extract_stereo(&[0.5, 0.7], 1), vec![0.5, 0.5, 0.7, 0.7]);
    }

    #[test]
    fn extract_mono_centered_averages_channels() {
        assert_eq!(
            extract_mono_centered(&[0.2, 0.6, -0.2, 0.2], 2),
            vec![0.4, 0.4, 0.0, 0.0]
        );
        assert_eq!(
            extract_mono_centered(&[0.5, 0.7], 1),
            vec![0.5, 0.5, 0.7, 0.7]
        );
    }

    #[test]
    fn apply_gain_scales_and_clamps_samples() {
        let mut samples = vec![-0.6, 0.25, 0.75];
        apply_gain(&mut samples, 2.0);
        assert_eq!(samples, vec![-1.0, 0.5, 1.0]);
    }

    #[test]
    fn discontinuity_fade_crosses_buffers_and_waits_through_digital_silence() {
        let mut fade = DiscontinuityFade::new();
        let mut silence = pairs(960, 0.0);
        fade.apply(&mut silence);
        assert_eq!(fade.remaining_pairs, DISCONTINUITY_FADE_PAIRS);

        let mut first = pairs(960, 1.0);
        fade.apply(&mut first);
        assert_eq!(&first[..2], &[0.0, 0.0]);
        assert!((first[first.len() - 1] - 959.0 / 1_919.0).abs() < 1e-6);

        let mut second = pairs(960, 1.0);
        fade.apply(&mut second);
        assert!((second[0] - 960.0 / 1_919.0).abs() < 1e-6);
        assert_eq!(&second[second.len() - 2..], &[1.0, 1.0]);
        assert_eq!(fade.remaining_pairs, 0);

        let mut steady = pairs(4, 0.75);
        fade.apply(&mut steady);
        assert!(steady.iter().all(|sample| *sample == 0.75));

        fade.restart();
        fade.apply(&mut steady);
        assert_eq!(&steady[..2], &[0.0, 0.0]);
    }

    #[test]
    fn discontinuity_fade_waits_through_leading_silence_inside_a_live_buffer() {
        let mut fade = DiscontinuityFade::new();
        let mut mixed = pairs(480, 0.0);
        mixed.extend(pairs(480, 1.0));

        fade.apply(&mut mixed);

        assert_eq!(fade.remaining_pairs, DISCONTINUITY_FADE_PAIRS - 480);
        assert!(mixed[..960].iter().all(|sample| *sample == 0.0));
        assert_eq!(&mixed[960..962], &[0.0, 0.0]);
        assert!((mixed[mixed.len() - 1] - 479.0 / 1_919.0).abs() < 1e-6);
    }

    #[test]
    fn resample_stereo_linear_preserves_stereo_pairs() {
        let samples = vec![0.0, 1.0, 0.5, 0.5, 1.0, 0.0, 0.5, -0.5];
        assert_eq!(resample_stereo_linear(&samples, 48_000, 48_000), samples);

        let down = resample_stereo_linear(&samples, 48_000, 24_000);
        assert_eq!(down, vec![0.0, 1.0, 1.0, 0.0]);

        let up = resample_stereo_linear(&[0.0, 1.0, 1.0, 0.0], 24_000, 48_000);
        assert_eq!(up.len(), 8);
        assert_eq!(&up[..4], &[0.0, 1.0, 0.5, 0.5]);
    }

    #[test]
    fn caller_owned_conversion_buffers_keep_stable_capacity() {
        let input = vec![0.25; 960 * 2];
        let mut stereo = Vec::with_capacity(input.len());
        let stereo_capacity = stereo.capacity();
        let resampled_capacity = maximum_resampled_stereo_samples(960, 44_100, 48_000).unwrap();
        let mut resampled = Vec::with_capacity(resampled_capacity);
        let mut converter = StereoResampler::new(44_100, 48_000);

        for _ in 0..100 {
            extract_stereo_into(&input, 2, &mut stereo);
            converter.resample_into(&stereo, &mut resampled);
            assert_eq!(stereo.capacity(), stereo_capacity);
            assert_eq!(resampled.capacity(), resampled_capacity);
            assert!(resampled.len() <= resampled_capacity);
        }
    }

    #[test]
    fn stateful_resampler_carries_fractional_position_across_chunks() {
        let mut resampler = StereoResampler::new(44_100, 48_000);
        let chunk = pairs(100, 0.25);
        let mut output_frames = 0usize;
        for _ in 0..1000 {
            output_frames += resampler.resample(&chunk).len() / 2;
        }

        let input_frames = 100_000f64;
        let expected = (input_frames * 48_000.0 / 44_100.0).ceil() as usize;
        assert_eq!(output_frames, expected);
    }

    #[test]
    fn continuous_device_packets_preserve_nominal_sample_cadence() {
        let mut timeline = DevicePacketTimeline::new();
        let mut assembler = LoopbackAssembler::new();
        assembler.push_chunk(0.0, &[]);

        for index in 0..300 {
            let pts_s = index as f64 * 510.0 / SAMPLE_RATE;
            let chunk = pairs(512, 0.5);
            match timeline.placement(Some(pts_s)) {
                DevicePacketPlacement::Timestamped(anchor_pts_s) => {
                    let outcome = assembler.push_chunk(anchor_pts_s, &chunk);
                    assert_eq!(outcome.late_reanchor_s, None);
                }
                DevicePacketPlacement::Contiguous => assembler.push_contiguous_chunk(&chunk),
            }
        }

        assert_eq!(assembler.written_pairs(), 300 * 512);
        let actual_end = assembler
            .next_chunk_pts_s
            .expect("timeline remains anchored");
        let expected_end = 300.0 * 512.0 / SAMPLE_RATE;
        assert!((actual_end - expected_end).abs() < 1e-9);
    }

    #[test]
    fn synthesized_idle_requires_the_next_packet_to_reanchor() {
        let mut timeline = DevicePacketTimeline::new();
        assert_eq!(
            timeline.placement(Some(1.0)),
            DevicePacketPlacement::Timestamped(1.0)
        );
        assert_eq!(
            timeline.placement(Some(1.01)),
            DevicePacketPlacement::Contiguous
        );
        assert!(!timeline.note_synthesized_silence(0.1 - 1e-6));
        assert_eq!(
            timeline.placement(Some(1.02)),
            DevicePacketPlacement::Contiguous
        );
        assert!(timeline.note_synthesized_silence(0.1));
        assert_eq!(
            timeline.placement(Some(1.03)),
            DevicePacketPlacement::Timestamped(1.03)
        );
        assert_eq!(
            timeline.placement(Some(1.04)),
            DevicePacketPlacement::Contiguous
        );

        timeline.require_timestamp_anchor();
        assert_eq!(timeline.placement(None), DevicePacketPlacement::Contiguous);
        assert_eq!(
            timeline.placement(Some(1.05)),
            DevicePacketPlacement::Timestamped(1.05)
        );
    }

    #[test]
    fn reactivation_waits_for_the_retry_interval_after_a_failure() {
        let start = Instant::now();
        let mut reactivation = DeviceReactivation::new(Duration::from_secs(1));
        assert!(reactivation.is_live());
        assert!(!reactivation.retry_due(start));

        assert!(reactivation.note_lost(start));
        assert!(!reactivation.is_live());
        assert!(!reactivation.retry_due(start));
        assert!(!reactivation.retry_due(start + Duration::from_millis(999)));
        assert!(reactivation.retry_due(start + Duration::from_secs(1)));
    }

    #[test]
    fn repeated_failures_extend_retry_without_resetting_the_outage() {
        let start = Instant::now();
        let mut reactivation = DeviceReactivation::new(Duration::from_secs(1));
        assert!(reactivation.note_lost(start));
        // A failing retry at t+1s schedules the next attempt but the outage
        // still measures from the first failure.
        reactivation.note_retry_failed(start + Duration::from_secs(1));
        assert!(!reactivation.retry_due(start + Duration::from_millis(1999)));
        assert!(reactivation.retry_due(start + Duration::from_secs(2)));
        let outage = reactivation
            .note_recovered(start + Duration::from_millis(2500))
            .expect("an outage was in progress");
        assert_eq!(outage, Duration::from_millis(2500));
        assert!(reactivation.is_live());
        assert!(!reactivation.retry_due(start + Duration::from_secs(60)));
    }

    #[test]
    fn repeated_device_errors_before_the_deadline_do_not_postpone_retry() {
        let start = Instant::now();
        let mut reactivation = DeviceReactivation::new(Duration::from_secs(1));
        assert!(reactivation.note_lost(start));

        for elapsed_ms in [16, 33, 250, 999] {
            assert!(!reactivation.note_lost(start + Duration::from_millis(elapsed_ms)));
        }

        assert!(reactivation.retry_due(start + Duration::from_secs(1)));
    }

    #[test]
    fn failed_retry_is_rescheduled_from_when_the_attempt_finishes() {
        let start = Instant::now();
        let mut reactivation = DeviceReactivation::new(Duration::from_secs(1));
        assert!(reactivation.note_lost(start));
        let attempt_finished = start + Duration::from_millis(2500);

        reactivation.note_retry_failed(attempt_finished);

        assert!(!reactivation.retry_due(attempt_finished + Duration::from_millis(999)));
        assert!(reactivation.retry_due(attempt_finished + Duration::from_secs(1)));
    }

    #[test]
    fn recovery_without_an_outage_reports_nothing() {
        let start = Instant::now();
        let mut reactivation = DeviceReactivation::new(Duration::from_secs(1));
        assert_eq!(reactivation.note_recovered(start), None);
    }
}
