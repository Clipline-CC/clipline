use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Barrier};
use std::time::Duration;

use clipline_settings::{
    BoundedProbePayload, ProbeAdmissionError, ProbeExecutor, ProbeKind, ProbeOutcome,
    ProbeRequestGeneration, ProbeSessionFence, ProbeSessionOwner, ProbeSubmitError,
    ProbeSubmitOutcome, ProbeToken, ProbeTokenFence, SettingsAttachmentGeneration,
    SettingsForegroundGeneration, SettingsSessionGeneration, SettingsTab, MAX_PROBE_ERROR_BYTES,
    MAX_PROBE_WORK_BYTES, PROBE_RESULT_CAPACITY, PROBE_WORKER_COUNT,
};

#[derive(Debug, PartialEq, Eq)]
struct Payload(String);

impl BoundedProbePayload for Payload {
    fn validate_bounds(&self) -> Result<(), String> {
        if self.0.len() > 32 {
            Err(format!("payload label is {} bytes", self.0.len()))
        } else {
            Ok(())
        }
    }
}

fn owner(session: u64, attachment: u64, foreground: u64) -> ProbeSessionOwner {
    ProbeSessionOwner::new(
        SettingsSessionGeneration::new(session),
        SettingsAttachmentGeneration::new(attachment),
        SettingsForegroundGeneration::new(foreground),
    )
}

#[test]
fn only_the_active_tab_can_mint_checked_exact_tokens() {
    let fence = ProbeSessionFence::new(owner(1, 2, 3), SettingsTab::Capture);
    let first = fence.request(ProbeKind::Displays).unwrap();
    let second = fence.request(ProbeKind::Displays).unwrap();
    assert_eq!(first.request_generation.get(), 1);
    assert_eq!(second.request_generation.get(), 2);
    assert!(!fence.is_current(first));
    assert!(fence.is_current(second));
    assert_eq!(
        fence.request(ProbeKind::Encoders),
        Err(ProbeAdmissionError::InactiveTab {
            kind: ProbeKind::Encoders,
            active_tab: SettingsTab::Capture,
        })
    );

    fence.set_active_tab(SettingsTab::Recording);
    assert!(!fence.is_current(second));
    assert!(fence.request(ProbeKind::Encoders).is_ok());
    fence.disconnect();
    assert_eq!(
        fence.request(ProbeKind::Encoders),
        Err(ProbeAdmissionError::Disconnected)
    );

    let exhausted = ProbeSessionFence::open_with_request_generation(
        owner(9, 9, 9),
        SettingsTab::Storage,
        u64::MAX,
    );
    assert_eq!(
        exhausted.request(ProbeKind::Storage),
        Err(ProbeAdmissionError::GenerationExhausted {
            kind: ProbeKind::Storage,
        })
    );
}

#[test]
fn opening_executor_does_no_probe_work_before_explicit_admission() {
    let fence = Arc::new(ProbeSessionFence::new(owner(1, 1, 1), SettingsTab::Capture));
    let runs = Arc::new(AtomicUsize::new(0));
    let (mut executor, results) = ProbeExecutor::<Payload>::new(fence).unwrap();

    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(runs.load(Ordering::Acquire), 0);
    assert!(results.is_empty());
    executor.shutdown();
    assert_eq!(PROBE_WORKER_COUNT, 2);
}

#[test]
fn stale_after_activation_never_publishes() {
    let fence = Arc::new(ProbeSessionFence::new(owner(1, 1, 1), SettingsTab::Capture));
    let (mut executor, results) = ProbeExecutor::<Payload>::new(fence.clone()).unwrap();
    let token = fence.request(ProbeKind::Displays).unwrap();
    let (activated_tx, activated_rx) = mpsc::channel();
    let (continue_tx, continue_rx) = mpsc::channel();
    executor
        .submit(token, 0, move |context| {
            activated_tx.send(()).unwrap();
            continue_rx.recv().unwrap();
            context.checkpoint_after_activation()?;
            Ok(Payload("stale".into()))
        })
        .unwrap();
    activated_rx.recv().unwrap();
    fence.replace_owner(owner(2, 2, 2), SettingsTab::Capture);
    continue_tx.send(()).unwrap();

    assert!(results.wait_recv(Duration::from_millis(50)).is_none());
    executor.shutdown();
}

#[test]
fn ten_thousand_request_storm_keeps_only_active_and_latest_pending() {
    let fence = Arc::new(ProbeSessionFence::new(owner(1, 1, 1), SettingsTab::Capture));
    let (mut executor, results) = ProbeExecutor::<Payload>::new(fence.clone()).unwrap();
    let runs = Arc::new(AtomicUsize::new(0));
    let (active_tx, active_rx) = mpsc::channel();
    let gate = Arc::new(Barrier::new(2));

    let first = fence.request(ProbeKind::Displays).unwrap();
    let first_runs = runs.clone();
    let first_gate = gate.clone();
    executor
        .submit(first, 0, move |context| {
            first_runs.fetch_add(1, Ordering::AcqRel);
            active_tx.send(()).unwrap();
            first_gate.wait();
            context.checkpoint_after_activation()?;
            Ok(Payload("first".into()))
        })
        .unwrap();
    active_rx.recv().unwrap();

    let mut latest = first;
    for index in 0..10_000_u64 {
        latest = fence.request(ProbeKind::Displays).unwrap();
        let pending_runs = runs.clone();
        let outcome = executor
            .submit(latest, 0, move |context| {
                pending_runs.fetch_add(1, Ordering::AcqRel);
                context.checkpoint_after_activation()?;
                Ok(Payload(format!("latest-{index}")))
            })
            .unwrap();
        assert!(matches!(
            outcome,
            ProbeSubmitOutcome::Queued | ProbeSubmitOutcome::Replaced
        ));
    }
    gate.wait();

    let result = results.wait_recv(Duration::from_secs(2)).unwrap();
    assert_eq!(result.token, latest);
    assert_eq!(runs.load(Ordering::Acquire), 2);
    assert!(results.is_empty());
    executor.shutdown();
}

struct AlwaysCurrent;

impl ProbeTokenFence for AlwaysCurrent {
    fn is_current(&self, _token: ProbeToken) -> bool {
        true
    }
}

fn token(owner: ProbeSessionOwner, generation: u64) -> ProbeToken {
    ProbeToken {
        owner,
        kind: ProbeKind::Displays,
        request_generation: ProbeRequestGeneration::new(generation),
    }
}

#[test]
fn different_owner_pending_lane_reports_full_without_cross_owner_coalescing() {
    let (mut executor, _results) = ProbeExecutor::<Payload>::new(Arc::new(AlwaysCurrent)).unwrap();
    let (active_tx, active_rx) = mpsc::channel();
    let gate = Arc::new(Barrier::new(2));
    let first_gate = gate.clone();
    executor
        .submit(token(owner(1, 1, 1), 1), 0, move |context| {
            active_tx.send(()).unwrap();
            first_gate.wait();
            context.checkpoint_after_activation()?;
            Ok(Payload("first".into()))
        })
        .unwrap();
    active_rx.recv().unwrap();
    executor
        .submit(token(owner(2, 2, 2), 1), 0, |context| {
            context.checkpoint_after_activation()?;
            Ok(Payload("second".into()))
        })
        .unwrap();
    assert_eq!(
        executor.submit(token(owner(3, 3, 3), 1), 0, |context| {
            context.checkpoint_after_activation()?;
            Ok(Payload("third".into()))
        }),
        Err(ProbeSubmitError::Full {
            kind: ProbeKind::Displays,
        })
    );
    gate.wait();
    executor.shutdown();
}

#[test]
fn errors_payloads_and_missing_activation_checkpoints_fail_closed_and_bounded() {
    let fence = Arc::new(ProbeSessionFence::new(
        owner(1, 1, 1),
        SettingsTab::Recording,
    ));
    let (mut executor, results) = ProbeExecutor::<Payload>::new(fence.clone()).unwrap();

    let oversized_work = fence.request(ProbeKind::Encoders).unwrap();
    assert_eq!(
        executor.submit(oversized_work, MAX_PROBE_WORK_BYTES + 1, |_| {
            Ok(Payload("never".into()))
        }),
        Err(ProbeSubmitError::WorkTooLarge {
            actual: MAX_PROBE_WORK_BYTES + 1,
            maximum: MAX_PROBE_WORK_BYTES,
        })
    );

    let missing = fence.request(ProbeKind::Encoders).unwrap();
    executor
        .submit(missing, 0, |_| Ok(Payload("ok".into())))
        .unwrap();
    let missing = results.wait_recv(Duration::from_secs(1)).unwrap();
    assert!(matches!(
        missing.outcome,
        ProbeOutcome::Failed(ref error) if error.contains("post-activation")
    ));

    let oversized = fence.request(ProbeKind::Encoders).unwrap();
    executor
        .submit(oversized, 0, |context| {
            context.checkpoint_after_activation()?;
            Ok(Payload("x".repeat(33)))
        })
        .unwrap();
    let oversized = results.wait_recv(Duration::from_secs(1)).unwrap();
    assert!(matches!(oversized.outcome, ProbeOutcome::Failed(_)));

    let huge_error = fence.request(ProbeKind::Encoders).unwrap();
    executor
        .submit(huge_error, 0, |context| {
            context.checkpoint_after_activation()?;
            Err("é".repeat(MAX_PROBE_ERROR_BYTES))
        })
        .unwrap();
    let huge_error = results.wait_recv(Duration::from_secs(1)).unwrap();
    let ProbeOutcome::Failed(error) = huge_error.outcome else {
        panic!("huge error must fail");
    };
    assert_eq!(error.len(), MAX_PROBE_ERROR_BYTES);
    assert!(error.is_char_boundary(error.len()));
    executor.shutdown();
}

#[test]
fn result_delivery_is_one_outstanding_value_per_exact_owner_and_kind() {
    let fence = Arc::new(ProbeSessionFence::new(owner(1, 1, 1), SettingsTab::Capture));
    let (mut executor, results) = ProbeExecutor::<Payload>::new(fence.clone()).unwrap();
    let first = fence.request(ProbeKind::Displays).unwrap();
    executor
        .submit(first, 0, |context| {
            context.checkpoint_after_activation()?;
            Ok(Payload("first".into()))
        })
        .unwrap();
    std::thread::sleep(Duration::from_millis(20));
    let second = fence.request(ProbeKind::Displays).unwrap();
    executor
        .submit(second, 0, |context| {
            context.checkpoint_after_activation()?;
            Ok(Payload("second".into()))
        })
        .unwrap();
    std::thread::sleep(Duration::from_millis(20));

    assert_eq!(results.len(), 1);
    let result = results.try_recv().unwrap();
    assert_eq!(result.token, second);
    assert!(results.is_empty());
    assert_eq!(PROBE_RESULT_CAPACITY, ProbeKind::COUNT);
    executor.shutdown();
}

#[test]
fn shutdown_drops_pending_work_joins_workers_and_rejects_new_admission() {
    let fence = Arc::new(ProbeSessionFence::new(owner(1, 1, 1), SettingsTab::Capture));
    let (mut executor, _results) = ProbeExecutor::<Payload>::new(fence.clone()).unwrap();
    let token = fence.request(ProbeKind::Displays).unwrap();
    let ran = Arc::new(AtomicBool::new(false));
    let work_ran = ran.clone();
    executor
        .submit(token, 0, move |context| {
            work_ran.store(true, Ordering::Release);
            context.checkpoint_after_activation()?;
            Ok(Payload("done".into()))
        })
        .unwrap();
    executor.shutdown();
    assert_eq!(executor.worker_count(), 0);
    assert_eq!(
        executor.submit(token, 0, |_| Ok(Payload("late".into()))),
        Err(ProbeSubmitError::Disconnected)
    );
}
