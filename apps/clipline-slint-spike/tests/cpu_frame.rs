use clipline_playback::{PipelineToken, WorkGeneration};
use clipline_slint_spike::cpu_frame::{cpu_frame_mailbox, CpuFrameError};

fn token(open: u64, seek: u64, revision: u64) -> PipelineToken {
    PipelineToken::new(WorkGeneration::new(open, seek), revision)
}

#[test]
fn mailbox_replaces_only_the_latest_frame_and_reuses_one_buffer() {
    let (mut producer, mut consumer) = cpu_frame_mailbox();
    let active = token(1, 0, 1);
    producer.clear(active);

    let mut first = producer.acquire(active, 2, 2).unwrap();
    first.pixels_mut().fill(11);
    producer.commit(first, 100).unwrap();
    let first_capacity = producer.telemetry().rgb_capacity;

    let mut second = producer.acquire(active, 2, 2).unwrap();
    second.pixels_mut().fill(22);
    producer.commit(second, 80).unwrap();

    let frame = consumer.take_latest(active).unwrap();
    assert_eq!(frame.pixels(), &[22; 12]);
    assert_eq!(frame.copy_time_100ns(), 80);
    assert_eq!(producer.telemetry().rgb_capacity, first_capacity);
    assert_eq!(producer.telemetry().allocation_count, 1);
    assert_eq!(producer.telemetry().replaced_frames, 1);
    assert_eq!(producer.telemetry().pending_high_water, 1);
    consumer.recycle(frame);
}

#[test]
fn stale_tokens_are_rejected_and_dimensions_are_hard_bounded() {
    let (mut producer, mut consumer) = cpu_frame_mailbox();
    let old = token(1, 0, 1);
    let current = token(1, 1, 0);
    producer.clear(old);
    let stale = producer.acquire(old, 2, 2).unwrap();
    producer.clear(current);
    assert_eq!(producer.commit(stale, 1), Err(CpuFrameError::StaleToken));
    assert!(consumer.take_latest(current).is_none());

    assert!(matches!(
        producer.acquire(current, 1, 2),
        Err(CpuFrameError::InvalidDimensions { .. })
    ));
    assert!(matches!(
        producer.acquire(current, 4_096, 4_096),
        Err(CpuFrameError::FrameTooLarge { .. })
    ));
}

#[test]
fn producer_backpressures_until_the_event_loop_recycles_the_buffer() {
    let (mut producer, mut consumer) = cpu_frame_mailbox();
    let active = token(1, 0, 0);
    producer.clear(active);
    let frame = producer.acquire(active, 2, 2).unwrap();
    producer.commit(frame, 1).unwrap();
    let frame = consumer.take_latest(active).unwrap();

    assert!(matches!(
        producer.acquire(active, 2, 2),
        Err(CpuFrameError::Backpressured)
    ));
    consumer.recycle(frame);
    assert!(producer.acquire(active, 2, 2).is_ok());
}
