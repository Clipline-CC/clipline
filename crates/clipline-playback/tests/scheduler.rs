use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use clipline_playback::{
    plan_audio_fill, AdmitOutcome, AudioAvailability, AudioFillPlan, BackendError, ClockError,
    DecodedVideoFrame, EndOfStreamTracker, FramePublisher, FrameScheduler, MonotonicTime100ns,
    PipelineToken, PublicationReceipt, RawAudioClock, RebasedAudioClock, SeekTarget,
    TimelineDuration, TimelinePosition, WorkGeneration, MAX_AUDIO_WRITE_FRAMES,
};

const TOKEN: PipelineToken = PipelineToken::new(WorkGeneration::new(1, 0), 0);

#[derive(Debug)]
struct TestSurface {
    id: usize,
    drops: Arc<AtomicUsize>,
}

impl Drop for TestSurface {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct TestPublisher {
    published: Vec<usize>,
    cleared: Vec<PipelineToken>,
    receipt: PublicationReceipt,
}

impl FramePublisher<TestSurface> for TestPublisher {
    fn publish(
        &mut self,
        frame: DecodedVideoFrame<TestSurface>,
    ) -> Result<PublicationReceipt, BackendError> {
        self.published.push(frame.surface().id);
        Ok(self.receipt)
    }

    fn clear(&mut self, token: PipelineToken) -> Result<(), BackendError> {
        self.cleared.push(token);
        Ok(())
    }
}

#[test]
fn publication_backpressure_and_occlusion_are_not_counted_as_presented() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut scheduler = FrameScheduler::new(TOKEN, SeekTarget::new(pos(0), 0));
    let mut publisher = TestPublisher {
        receipt: PublicationReceipt::Backpressured,
        ..TestPublisher::default()
    };
    scheduler
        .admit(frame(0, 0, 1, TOKEN, &drops), pos(0))
        .unwrap();
    assert!(!scheduler
        .tick(
            pos(0),
            &mut publisher,
            || panic!("a frame that was not presented has no publication clock"),
            MonotonicTime100ns::new(0),
        )
        .unwrap());
    assert_eq!(scheduler.metrics().presented_frames, 0);
    assert_eq!(scheduler.metrics().presentation_backpressured_frames, 1);
    assert_eq!(scheduler.metrics().scheduler_dropped_frames, 1);
    assert_eq!(scheduler.metrics().late_or_dropped_frames, 1);

    publisher.receipt = PublicationReceipt::Occluded;
    scheduler
        .admit(frame(1, 1, 1, TOKEN, &drops), pos(1))
        .unwrap();
    assert!(!scheduler
        .tick(
            pos(1),
            &mut publisher,
            || panic!("an occluded frame has no publication clock"),
            MonotonicTime100ns::new(1),
        )
        .unwrap());
    assert_eq!(scheduler.metrics().presented_frames, 0);
    assert_eq!(scheduler.metrics().presentation_occluded_frames, 1);
    assert_eq!(scheduler.metrics().scheduler_dropped_frames, 1);
    assert_eq!(drops.load(Ordering::SeqCst), 2);
}

fn pos(ticks: u64) -> TimelinePosition {
    TimelinePosition::new(ticks)
}

fn duration(ticks: u64) -> TimelineDuration {
    TimelineDuration::new(ticks).unwrap()
}

fn frame(
    id: usize,
    pts: u64,
    duration_ticks: u64,
    token: PipelineToken,
    drops: &Arc<AtomicUsize>,
) -> DecodedVideoFrame<TestSurface> {
    DecodedVideoFrame::new(
        TestSurface {
            id,
            drops: Arc::clone(drops),
        },
        id,
        pos(pts),
        duration(duration_ticks),
        token,
    )
}

#[test]
fn exact_duration_boundary_is_not_late_and_one_tick_later_is() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut publisher = TestPublisher::default();
    let mut scheduler = FrameScheduler::new(TOKEN, SeekTarget::new(pos(0), 0));

    assert_eq!(
        scheduler
            .admit(frame(1, 100, 10, TOKEN, &drops), pos(110))
            .unwrap(),
        AdmitOutcome::Queued
    );
    assert!(scheduler
        .tick(
            pos(110),
            &mut publisher,
            || Ok(pos(110)),
            MonotonicTime100ns::new(1_000)
        )
        .unwrap());
    assert_eq!(scheduler.metrics().late_frames, 0);
    assert_eq!(scheduler.metrics().latest_av_error_ticks, Some(10));

    assert_eq!(
        scheduler
            .admit(frame(2, 200, 10, TOKEN, &drops), pos(211))
            .unwrap(),
        AdmitOutcome::Queued
    );
    assert!(scheduler
        .tick(
            pos(211),
            &mut publisher,
            || Ok(pos(211)),
            MonotonicTime100ns::new(2_000)
        )
        .unwrap());

    let metrics = scheduler.metrics();
    assert_eq!(publisher.published, vec![1, 2]);
    assert_eq!(metrics.decoded_eligible_frames, 2);
    assert_eq!(metrics.presented_frames, 2);
    assert_eq!(metrics.late_frames, 1);
    assert_eq!(metrics.late_or_dropped_frames, 1);
    assert_eq!(metrics.late_drop_ratio(), 0.5);
    assert_eq!(drops.load(Ordering::SeqCst), 2);
}

#[test]
fn newest_due_frame_wins_and_only_one_future_surface_is_retained() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut scheduler = FrameScheduler::new(TOKEN, SeekTarget::new(pos(0), 0));
    let mut publisher = TestPublisher::default();

    assert_eq!(
        scheduler
            .admit(frame(1, 10, 10, TOKEN, &drops), pos(30))
            .unwrap(),
        AdmitOutcome::Queued
    );
    assert_eq!(
        scheduler
            .admit(frame(2, 20, 10, TOKEN, &drops), pos(30))
            .unwrap(),
        AdmitOutcome::DroppedOlderDue
    );
    assert_eq!(scheduler.pending_frames(), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(scheduler
        .tick(
            pos(30),
            &mut publisher,
            || Ok(pos(30)),
            MonotonicTime100ns::new(30)
        )
        .unwrap());

    assert_eq!(
        scheduler
            .admit(frame(3, 40, 10, TOKEN, &drops), pos(30))
            .unwrap(),
        AdmitOutcome::Queued
    );
    let rejected = scheduler
        .admit(frame(4, 50, 10, TOKEN, &drops), pos(30))
        .unwrap_err();
    assert_eq!(rejected.sample_index(), 4);
    drop(rejected);
    assert_eq!(scheduler.pending_frames(), 1);
    assert!(!scheduler
        .tick(
            pos(39),
            &mut publisher,
            || Ok(pos(39)),
            MonotonicTime100ns::new(39)
        )
        .unwrap());
    assert!(scheduler
        .tick(
            pos(40),
            &mut publisher,
            || Ok(pos(40)),
            MonotonicTime100ns::new(40)
        )
        .unwrap());

    assert_eq!(publisher.published, vec![2, 3]);
    assert_eq!(scheduler.metrics().scheduler_dropped_frames, 1);
    assert_eq!(scheduler.metrics().late_or_dropped_frames, 1);
    assert_eq!(scheduler.metrics().decoded_eligible_frames, 3);
}

#[test]
fn generation_revision_and_preroll_are_rejected_before_quality_metrics() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut scheduler = FrameScheduler::new(TOKEN, SeekTarget::new(pos(100), 3));
    let old_revision = PipelineToken::new(TOKEN.work(), 7);
    let old_generation = PipelineToken::new(WorkGeneration::new(0, 9), 0);

    assert_eq!(
        scheduler
            .admit(frame(1, 100, 10, old_revision, &drops), pos(100))
            .unwrap(),
        AdmitOutcome::Stale
    );
    assert_eq!(
        scheduler
            .admit(frame(2, 100, 10, old_generation, &drops), pos(100))
            .unwrap(),
        AdmitOutcome::Stale
    );
    assert_eq!(
        scheduler
            .admit(frame(2, 99, 10, TOKEN, &drops), pos(100))
            .unwrap(),
        AdmitOutcome::Preroll
    );

    let metrics = scheduler.metrics();
    assert_eq!(metrics.stale_results, 2);
    assert_eq!(metrics.preroll_frames, 1);
    assert_eq!(metrics.decoded_eligible_frames, 0);
    assert_eq!(metrics.late_drop_ratio(), 0.0);
    assert_eq!(scheduler.pending_frames(), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 3);
}

#[test]
fn seek_latency_belongs_only_to_the_final_token_target_frame() {
    let drops = Arc::new(AtomicUsize::new(0));
    let superseded = PipelineToken::new(WorkGeneration::new(1, 1), 0);
    let final_token = PipelineToken::new(WorkGeneration::new(1, 2), 0);
    let mut scheduler = FrameScheduler::new(TOKEN, SeekTarget::new(pos(0), 0));
    let mut publisher = TestPublisher::default();

    scheduler.begin_seek(
        superseded,
        SeekTarget::new(pos(200), 1),
        MonotonicTime100ns::new(1_000),
    );
    scheduler.begin_seek(
        final_token,
        SeekTarget::new(pos(500), 3),
        MonotonicTime100ns::new(1_500),
    );
    assert_eq!(
        scheduler
            .admit(frame(1, 200, 10, superseded, &drops), pos(500))
            .unwrap(),
        AdmitOutcome::Stale
    );
    assert_eq!(
        scheduler
            .admit(frame(2, 499, 10, final_token, &drops), pos(500))
            .unwrap(),
        AdmitOutcome::Preroll
    );
    scheduler
        .admit(frame(3, 500, 10, final_token, &drops), pos(500))
        .unwrap();
    assert!(scheduler
        .tick(
            pos(500),
            &mut publisher,
            || Ok(pos(500)),
            MonotonicTime100ns::new(1_575)
        )
        .unwrap());

    assert_eq!(publisher.published, vec![3]);
    assert_eq!(scheduler.metrics().latest_seek_latency_100ns, Some(75));
    assert_eq!(scheduler.metrics().settled_seeks, 1);
}

#[test]
fn seek_inside_a_frame_settles_on_the_containing_target_sample() {
    let drops = Arc::new(AtomicUsize::new(0));
    let token = PipelineToken::new(WorkGeneration::new(1, 1), 0);
    let target = SeekTarget::new(pos(105), 10);
    let mut scheduler = FrameScheduler::new(TOKEN, SeekTarget::new(pos(0), 0));
    let mut publisher = TestPublisher::default();
    scheduler.begin_seek(token, target, MonotonicTime100ns::new(10));

    assert_eq!(
        scheduler
            .admit(frame(9, 90, 10, token, &drops), pos(105))
            .unwrap(),
        AdmitOutcome::Preroll
    );
    assert_eq!(
        scheduler
            .admit(frame(10, 100, 10, token, &drops), pos(105))
            .unwrap(),
        AdmitOutcome::Queued
    );
    assert!(scheduler
        .tick(
            pos(105),
            &mut publisher,
            || Ok(pos(105)),
            MonotonicTime100ns::new(25)
        )
        .unwrap());

    assert_eq!(publisher.published, vec![10]);
    assert_eq!(scheduler.metrics().settled_seeks, 1);
    assert_eq!(scheduler.metrics().latest_seek_latency_100ns, Some(15));
}

#[test]
fn rebased_audio_clock_freezes_reanchors_and_rejects_unannounced_device_changes() {
    let mut clock = RebasedAudioClock::new(RawAudioClock::new(0, 48_000, 1).unwrap(), pos(1_000));
    assert_eq!(
        clock
            .sample(RawAudioClock::new(480, 48_000, 1).unwrap())
            .unwrap(),
        pos(1_480)
    );
    assert_eq!(
        clock
            .pause(RawAudioClock::new(960, 48_000, 1).unwrap())
            .unwrap(),
        pos(1_960)
    );
    assert_eq!(
        clock
            .sample(RawAudioClock::new(1_440, 48_000, 1).unwrap())
            .unwrap(),
        pos(1_960)
    );
    assert!(matches!(
        clock.resume(RawAudioClock::new(1_000, 48_000, 1).unwrap()),
        Err(ClockError::RawPositionRegressed { .. })
    ));
    clock
        .resume(RawAudioClock::new(2_000, 48_000, 1).unwrap())
        .unwrap();
    assert_eq!(
        clock
            .sample(RawAudioClock::new(2_480, 48_000, 1).unwrap())
            .unwrap(),
        pos(2_440)
    );

    assert!(matches!(
        clock.sample(RawAudioClock::new(1_999, 48_000, 1).unwrap()),
        Err(ClockError::RawPositionRegressed { .. })
    ));
    assert!(matches!(
        clock.sample(RawAudioClock::new(0, 48_000, 2).unwrap()),
        Err(ClockError::EndpointEpochChanged { .. })
    ));

    clock.rebase(RawAudioClock::new(0, 48_000, 2).unwrap(), pos(9_000));
    assert_eq!(
        clock
            .sample(RawAudioClock::new(240, 48_000, 2).unwrap())
            .unwrap(),
        pos(9_240)
    );
    clock.rebase(RawAudioClock::new(5_000, 48_000, 2).unwrap(), pos(3_000));
    assert_eq!(
        clock
            .sample(RawAudioClock::new(5_480, 48_000, 2).unwrap())
            .unwrap(),
        pos(3_480),
        "seek/flush must re-anchor even when the target moves backward"
    );
}

#[test]
fn histograms_fail_closed_on_overflow_and_clock_failures_are_counted() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut scheduler = FrameScheduler::new(TOKEN, SeekTarget::new(pos(0), 0));
    let mut publisher = TestPublisher::default();

    scheduler
        .admit(frame(0, 0, 1, TOKEN, &drops), pos(0))
        .unwrap();
    assert!(scheduler
        .tick(
            pos(0),
            &mut publisher,
            || Ok(pos(24_624)),
            MonotonicTime100ns::new(0)
        )
        .unwrap());
    assert_eq!(scheduler.metrics().av_error_histogram.overflow(), 1);
    assert_eq!(
        scheduler.metrics().av_error_histogram.percentile_millis(95),
        None
    );

    scheduler
        .admit(frame(1, 30_000, 1, TOKEN, &drops), pos(30_000))
        .unwrap();
    assert!(matches!(
        scheduler.tick(
            pos(30_000),
            &mut publisher,
            || Err(ClockError::TimelineOverflow),
            MonotonicTime100ns::new(0)
        ),
        Err(clipline_playback::SchedulerError::Clock(
            ClockError::TimelineOverflow
        ))
    ));
    assert_eq!(scheduler.metrics().presented_frames, 2);
    assert_eq!(scheduler.metrics().unmeasured_presentations, 1);
}

#[test]
fn cancelled_generation_is_removed_from_the_active_quality_denominator() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut scheduler = FrameScheduler::new(TOKEN, SeekTarget::new(pos(0), 0));
    scheduler
        .admit(frame(0, 100, 10, TOKEN, &drops), pos(0))
        .unwrap();
    assert_eq!(scheduler.metrics().decoded_eligible_frames, 1);

    let seek_token = PipelineToken::new(WorkGeneration::new(1, 1), 0);
    scheduler.begin_seek(
        seek_token,
        SeekTarget::new(pos(100), 0),
        MonotonicTime100ns::new(0),
    );
    assert_eq!(scheduler.metrics().decoded_eligible_frames, 0);
    assert_eq!(scheduler.metrics().cancelled_frames, 1);
    assert_eq!(scheduler.metrics().late_drop_ratio(), 0.0);
}

#[test]
fn zero_track_gap_and_audio_tail_silence_are_explicit_and_bounded() {
    assert_eq!(
        plan_audio_fill(pos(0), pos(48_000), 24_000, AudioAvailability::NoTracks),
        AudioFillPlan {
            pcm_frames: 0,
            silence_frames: MAX_AUDIO_WRITE_FRAMES,
        }
    );
    assert_eq!(
        plan_audio_fill(
            pos(1_000),
            pos(48_000),
            4_000,
            AudioAvailability::Gap { frames: 600 }
        ),
        AudioFillPlan {
            pcm_frames: 0,
            silence_frames: 600,
        }
    );
    assert_eq!(
        plan_audio_fill(pos(47_500), pos(48_000), 4_000, AudioAvailability::Ended),
        AudioFillPlan {
            pcm_frames: 0,
            silence_frames: 500,
        }
    );
    assert_eq!(
        plan_audio_fill(
            pos(10_000),
            pos(48_000),
            4_000,
            AudioAvailability::Pcm { frames: 960 }
        ),
        AudioFillPlan {
            pcm_frames: 960,
            silence_frames: 0,
        },
        "temporary decoder starvation must not be fabricated as a known gap"
    );
}

#[test]
fn eof_waits_for_the_final_video_interval_and_fires_once_for_short_clips() {
    let mut tracker = EndOfStreamTracker::new(pos(800));
    assert!(!tracker.update(pos(10_000)));
    tracker.mark_final_frame_presented();
    assert!(!tracker.update(pos(799)));
    assert!(tracker.update(pos(800)));
    assert!(!tracker.update(pos(900)));
    assert!(tracker.is_ended());
}
