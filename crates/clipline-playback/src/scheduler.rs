use thiserror::Error;

use crate::backend::{
    BackendError, DecodedVideoFrame, FramePublisher, MonotonicTime100ns, PipelineToken,
    PublicationReceipt, RawAudioClock, TimelinePosition, PLAYBACK_TIMELINE_HZ,
};
use crate::ClockError;

pub const MAX_AUDIO_WRITE_FRAMES: usize = 5_760;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioAvailability {
    Pcm { frames: usize },
    Gap { frames: usize },
    Ended,
    NoTracks,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioFillPlan {
    pub pcm_frames: usize,
    pub silence_frames: usize,
}

pub fn plan_audio_fill(
    clock: TimelinePosition,
    video_end: TimelinePosition,
    writable_frames: usize,
    availability: AudioAvailability,
) -> AudioFillPlan {
    let remaining = video_end.ticks().saturating_sub(clock.ticks());
    let remaining = usize::try_from(remaining).unwrap_or(usize::MAX);
    let bounded = writable_frames.min(MAX_AUDIO_WRITE_FRAMES).min(remaining);
    match availability {
        AudioAvailability::Pcm { frames } => AudioFillPlan {
            pcm_frames: frames.min(bounded),
            silence_frames: 0,
        },
        AudioAvailability::Gap { frames } => AudioFillPlan {
            pcm_frames: 0,
            silence_frames: frames.min(bounded),
        },
        AudioAvailability::Ended | AudioAvailability::NoTracks => AudioFillPlan {
            pcm_frames: 0,
            silence_frames: bounded,
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndOfStreamTracker {
    video_end: TimelinePosition,
    final_frame_presented: bool,
    ended: bool,
}

impl EndOfStreamTracker {
    pub const fn new(video_end: TimelinePosition) -> Self {
        Self {
            video_end,
            final_frame_presented: false,
            ended: false,
        }
    }

    pub fn mark_final_frame_presented(&mut self) {
        self.final_frame_presented = true;
    }

    pub fn update(&mut self, clock: TimelinePosition) -> bool {
        if self.ended || !self.final_frame_presented || clock < self.video_end {
            return false;
        }
        self.ended = true;
        true
    }

    pub const fn is_ended(self) -> bool {
        self.ended
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RebasedAudioClock {
    anchor_raw: RawAudioClock,
    last_raw_position: u64,
    anchor_timeline: TimelinePosition,
    last_timeline: TimelinePosition,
    paused: bool,
}

impl RebasedAudioClock {
    pub const fn new(anchor_raw: RawAudioClock, anchor_timeline: TimelinePosition) -> Self {
        Self {
            anchor_raw,
            last_raw_position: anchor_raw.position(),
            anchor_timeline,
            last_timeline: anchor_timeline,
            paused: false,
        }
    }

    pub fn sample(&mut self, raw: RawAudioClock) -> Result<TimelinePosition, ClockError> {
        self.validate_identity(raw)?;
        if raw.position() < self.last_raw_position {
            return Err(ClockError::RawPositionRegressed {
                anchor: self.last_raw_position,
                actual: raw.position(),
            });
        }
        self.last_raw_position = raw.position();
        if self.paused {
            return Ok(self.last_timeline);
        }
        if raw.position() < self.anchor_raw.position() {
            return Err(ClockError::RawPositionRegressed {
                anchor: self.anchor_raw.position(),
                actual: raw.position(),
            });
        }
        let raw_delta = raw.position() - self.anchor_raw.position();
        let timeline_delta = u128::from(raw_delta)
            .checked_mul(u128::from(PLAYBACK_TIMELINE_HZ))
            .ok_or(ClockError::TimelineOverflow)?
            / u128::from(raw.frequency());
        let timeline_delta =
            u64::try_from(timeline_delta).map_err(|_| ClockError::TimelineOverflow)?;
        let candidate = self
            .anchor_timeline
            .ticks()
            .checked_add(timeline_delta)
            .ok_or(ClockError::TimelineOverflow)?;
        self.last_timeline = TimelinePosition::new(candidate.max(self.last_timeline.ticks()));
        Ok(self.last_timeline)
    }

    pub fn pause(&mut self, raw: RawAudioClock) -> Result<TimelinePosition, ClockError> {
        let position = self.sample(raw)?;
        self.paused = true;
        Ok(position)
    }

    pub fn resume(&mut self, raw: RawAudioClock) -> Result<(), ClockError> {
        self.validate_identity(raw)?;
        if raw.position() < self.last_raw_position {
            return Err(ClockError::RawPositionRegressed {
                anchor: self.last_raw_position,
                actual: raw.position(),
            });
        }
        self.anchor_raw = raw;
        self.last_raw_position = raw.position();
        self.anchor_timeline = self.last_timeline;
        self.paused = false;
        Ok(())
    }

    pub fn rebase(&mut self, raw: RawAudioClock, timeline: TimelinePosition) {
        self.anchor_raw = raw;
        self.last_raw_position = raw.position();
        self.anchor_timeline = timeline;
        self.last_timeline = timeline;
        self.paused = false;
    }

    pub const fn position(self) -> TimelinePosition {
        self.last_timeline
    }

    pub const fn is_paused(self) -> bool {
        self.paused
    }

    fn validate_identity(&self, raw: RawAudioClock) -> Result<(), ClockError> {
        if raw.endpoint_epoch() != self.anchor_raw.endpoint_epoch() {
            return Err(ClockError::EndpointEpochChanged {
                expected: self.anchor_raw.endpoint_epoch(),
                actual: raw.endpoint_epoch(),
            });
        }
        if raw.frequency() != self.anchor_raw.frequency() {
            return Err(ClockError::FrequencyChanged {
                expected: self.anchor_raw.frequency(),
                actual: raw.frequency(),
            });
        }
        Ok(())
    }
}

pub const METRIC_HISTOGRAM_MAX_MILLIS: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricHistogram {
    buckets: [u64; METRIC_HISTOGRAM_MAX_MILLIS + 1],
    overflow: u64,
    total: u64,
}

impl Default for MetricHistogram {
    fn default() -> Self {
        Self {
            buckets: [0; METRIC_HISTOGRAM_MAX_MILLIS + 1],
            overflow: 0,
            total: 0,
        }
    }
}

impl MetricHistogram {
    fn observe_ceil_millis(&mut self, ticks: u64, ticks_per_millisecond: u64) {
        let millis = if ticks == 0 {
            0
        } else {
            ticks.saturating_add(ticks_per_millisecond - 1) / ticks_per_millisecond
        };
        self.total = self.total.saturating_add(1);
        if let Ok(bucket) = usize::try_from(millis) {
            if let Some(count) = self.buckets.get_mut(bucket) {
                *count = count.saturating_add(1);
                return;
            }
        }
        self.overflow = self.overflow.saturating_add(1);
    }

    pub fn percentile_millis(&self, percentile: u8) -> Option<u16> {
        if self.total == 0 || self.overflow != 0 || !(1..=100).contains(&percentile) {
            return None;
        }
        let rank = self
            .total
            .saturating_mul(u64::from(percentile))
            .saturating_add(99)
            / 100;
        let mut seen = 0_u64;
        for (millis, count) in self.buckets.iter().enumerate() {
            seen = seen.saturating_add(*count);
            if seen >= rank {
                return u16::try_from(millis).ok();
            }
        }
        None
    }

    pub const fn total(&self) -> u64 {
        self.total
    }

    pub const fn overflow(&self) -> u64 {
        self.overflow
    }

    fn accumulate(&mut self, next: &Self) {
        for (total, value) in self.buckets.iter_mut().zip(&next.buckets) {
            *total = total.saturating_add(*value);
        }
        self.overflow = self.overflow.saturating_add(next.overflow);
        self.total = self.total.saturating_add(next.total);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlaybackMetrics {
    pub decoded_eligible_frames: u64,
    pub presented_frames: u64,
    pub late_frames: u64,
    pub scheduler_dropped_frames: u64,
    pub presentation_backpressured_frames: u64,
    pub presentation_occluded_frames: u64,
    pub late_or_dropped_frames: u64,
    pub stale_results: u64,
    pub preroll_frames: u64,
    pub cancelled_frames: u64,
    pub unmeasured_presentations: u64,
    pub latest_av_error_ticks: Option<u64>,
    pub max_av_error_ticks: u64,
    pub av_error_histogram: MetricHistogram,
    pub latest_seek_latency_100ns: Option<u64>,
    pub seek_latency_histogram: MetricHistogram,
    pub settled_seeks: u64,
}

impl PlaybackMetrics {
    pub fn late_drop_ratio(&self) -> f64 {
        if self.decoded_eligible_frames == 0 {
            0.0
        } else {
            self.late_or_dropped_frames as f64 / self.decoded_eligible_frames as f64
        }
    }

    pub fn accumulate_generation(&mut self, next: &Self) {
        self.decoded_eligible_frames = self
            .decoded_eligible_frames
            .saturating_add(next.decoded_eligible_frames);
        self.presented_frames = self.presented_frames.saturating_add(next.presented_frames);
        self.late_frames = self.late_frames.saturating_add(next.late_frames);
        self.scheduler_dropped_frames = self
            .scheduler_dropped_frames
            .saturating_add(next.scheduler_dropped_frames);
        self.presentation_backpressured_frames = self
            .presentation_backpressured_frames
            .saturating_add(next.presentation_backpressured_frames);
        self.presentation_occluded_frames = self
            .presentation_occluded_frames
            .saturating_add(next.presentation_occluded_frames);
        self.late_or_dropped_frames = self
            .late_or_dropped_frames
            .saturating_add(next.late_or_dropped_frames);
        self.stale_results = self.stale_results.max(next.stale_results);
        self.preroll_frames = self.preroll_frames.saturating_add(next.preroll_frames);
        self.cancelled_frames = self.cancelled_frames.max(next.cancelled_frames);
        self.unmeasured_presentations = self
            .unmeasured_presentations
            .saturating_add(next.unmeasured_presentations);
        self.latest_av_error_ticks = next.latest_av_error_ticks.or(self.latest_av_error_ticks);
        self.max_av_error_ticks = self.max_av_error_ticks.max(next.max_av_error_ticks);
        self.av_error_histogram.accumulate(&next.av_error_histogram);
        self.latest_seek_latency_100ns = next
            .latest_seek_latency_100ns
            .or(self.latest_seek_latency_100ns);
        self.seek_latency_histogram
            .accumulate(&next.seek_latency_histogram);
        self.settled_seeks = self.settled_seeks.saturating_add(next.settled_seeks);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeekTarget {
    pub position: TimelinePosition,
    pub sample_index: usize,
}

impl SeekTarget {
    pub const fn new(position: TimelinePosition, sample_index: usize) -> Self {
        Self {
            position,
            sample_index,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmitOutcome {
    Queued,
    DroppedOlderDue,
    DroppedIncomingDue,
    Stale,
    Preroll,
}

#[derive(Debug, Error)]
pub enum SchedulerError {
    #[error(transparent)]
    Backend(#[from] BackendError),
    #[error(transparent)]
    Clock(#[from] ClockError),
}

#[derive(Debug, Clone, Copy)]
struct PendingSeek {
    token: PipelineToken,
    target: SeekTarget,
    accepted_at: MonotonicTime100ns,
}

#[derive(Debug)]
pub struct FrameScheduler<S> {
    token: PipelineToken,
    seek_target: SeekTarget,
    pending: Option<DecodedVideoFrame<S>>,
    pending_seek: Option<PendingSeek>,
    metrics: PlaybackMetrics,
}

impl<S> FrameScheduler<S> {
    pub fn new(token: PipelineToken, seek_target: SeekTarget) -> Self {
        Self {
            token,
            seek_target,
            pending: None,
            pending_seek: None,
            metrics: PlaybackMetrics::default(),
        }
    }

    pub fn begin_seek(
        &mut self,
        token: PipelineToken,
        seek_target: SeekTarget,
        accepted_at: MonotonicTime100ns,
    ) {
        if self.pending.take().is_some() {
            self.metrics.cancelled_frames = self.metrics.cancelled_frames.saturating_add(1);
        }
        self.reset_quality_metrics();
        self.token = token;
        self.seek_target = seek_target;
        self.pending_seek = Some(PendingSeek {
            token,
            target: seek_target,
            accepted_at,
        });
    }

    pub fn replace_pipeline(&mut self, token: PipelineToken, seek_target: SeekTarget) {
        if self.pending.take().is_some() {
            self.metrics.cancelled_frames = self.metrics.cancelled_frames.saturating_add(1);
        }
        self.reset_quality_metrics();
        self.token = token;
        self.seek_target = seek_target;
        self.pending_seek = None;
    }

    pub fn admit(
        &mut self,
        frame: DecodedVideoFrame<S>,
        clock: TimelinePosition,
    ) -> Result<AdmitOutcome, DecodedVideoFrame<S>> {
        if frame.token() != self.token {
            self.metrics.stale_results = self.metrics.stale_results.saturating_add(1);
            return Ok(AdmitOutcome::Stale);
        }
        if frame.sample_index() < self.seek_target.sample_index {
            self.metrics.preroll_frames = self.metrics.preroll_frames.saturating_add(1);
            return Ok(AdmitOutcome::Preroll);
        }

        let Some(existing) = self.pending.as_ref() else {
            self.metrics.decoded_eligible_frames =
                self.metrics.decoded_eligible_frames.saturating_add(1);
            self.pending = Some(frame);
            return Ok(AdmitOutcome::Queued);
        };
        let existing_due = existing.pts() <= clock;
        let incoming_due = frame.pts() <= clock;
        if !existing_due || !incoming_due {
            return Err(frame);
        }

        self.metrics.decoded_eligible_frames =
            self.metrics.decoded_eligible_frames.saturating_add(1);
        self.metrics.scheduler_dropped_frames =
            self.metrics.scheduler_dropped_frames.saturating_add(1);
        self.metrics.late_or_dropped_frames = self.metrics.late_or_dropped_frames.saturating_add(1);
        if frame.pts() >= existing.pts() {
            self.pending = Some(frame);
            Ok(AdmitOutcome::DroppedOlderDue)
        } else {
            Ok(AdmitOutcome::DroppedIncomingDue)
        }
    }

    pub fn tick<P, C>(
        &mut self,
        clock_before: TimelinePosition,
        publisher: &mut P,
        mut sample_after_publication: C,
        monotonic_now: MonotonicTime100ns,
    ) -> Result<bool, SchedulerError>
    where
        P: FramePublisher<S>,
        C: FnMut() -> Result<TimelinePosition, ClockError>,
    {
        let Some(frame) = self.pending.as_ref() else {
            return Ok(false);
        };
        if frame.pts() > clock_before {
            return Ok(false);
        }
        let frame = self.pending.take().expect("pending frame was just checked");
        let token = frame.token();
        let frame_sample_index = frame.sample_index();
        let pts = frame.pts();
        let duration = frame.duration();
        let end = pts
            .ticks()
            .checked_add(duration.ticks())
            .ok_or(ClockError::TimelineOverflow)?;
        match publisher.publish(frame)? {
            PublicationReceipt::Presented => {}
            PublicationReceipt::Backpressured => {
                self.metrics.presentation_backpressured_frames = self
                    .metrics
                    .presentation_backpressured_frames
                    .saturating_add(1);
                self.metrics.scheduler_dropped_frames =
                    self.metrics.scheduler_dropped_frames.saturating_add(1);
                self.metrics.late_or_dropped_frames =
                    self.metrics.late_or_dropped_frames.saturating_add(1);
                return Ok(false);
            }
            PublicationReceipt::Occluded => {
                self.metrics.presentation_occluded_frames =
                    self.metrics.presentation_occluded_frames.saturating_add(1);
                return Ok(false);
            }
        }
        self.metrics.presented_frames = self.metrics.presented_frames.saturating_add(1);

        let publication_clock = match sample_after_publication() {
            Ok(clock) => clock,
            Err(error) => {
                self.metrics.unmeasured_presentations =
                    self.metrics.unmeasured_presentations.saturating_add(1);
                return Err(error.into());
            }
        };
        let av_error = publication_clock.ticks().abs_diff(pts.ticks());
        self.metrics.latest_av_error_ticks = Some(av_error);
        self.metrics.max_av_error_ticks = self.metrics.max_av_error_ticks.max(av_error);
        self.metrics
            .av_error_histogram
            .observe_ceil_millis(av_error, u64::from(PLAYBACK_TIMELINE_HZ / 1_000));
        if publication_clock.ticks() > end {
            self.metrics.late_frames = self.metrics.late_frames.saturating_add(1);
            self.metrics.late_or_dropped_frames =
                self.metrics.late_or_dropped_frames.saturating_add(1);
        }

        if let Some(seek) = self.pending_seek {
            if seek.token == token && frame_sample_index == seek.target.sample_index {
                let latency = monotonic_now.elapsed_since(seek.accepted_at);
                self.metrics.latest_seek_latency_100ns = Some(latency);
                self.metrics
                    .seek_latency_histogram
                    .observe_ceil_millis(latency, 10_000);
                self.metrics.settled_seeks = self.metrics.settled_seeks.saturating_add(1);
                self.pending_seek = None;
            }
        }
        Ok(true)
    }

    pub fn pending_frames(&self) -> usize {
        usize::from(self.pending.is_some())
    }

    pub fn pending_sample_index(&self) -> Option<usize> {
        self.pending.as_ref().map(DecodedVideoFrame::sample_index)
    }

    pub const fn token(&self) -> PipelineToken {
        self.token
    }

    pub const fn metrics(&self) -> &PlaybackMetrics {
        &self.metrics
    }

    fn reset_quality_metrics(&mut self) {
        let stale_results = self.metrics.stale_results;
        let cancelled_frames = self.metrics.cancelled_frames;
        self.metrics = PlaybackMetrics {
            stale_results,
            cancelled_frames,
            ..PlaybackMetrics::default()
        };
    }
}
