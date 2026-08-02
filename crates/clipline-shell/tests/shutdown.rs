use clipline_shell::{
    ShellGeneration, ShutdownAcknowledgement, ShutdownCoordinator, ShutdownEffect, ShutdownError,
    ShutdownReason, ShutdownStage, MAX_SHUTDOWN_TIMEOUT_MS,
};

#[test]
fn shutdown_requires_every_acknowledgement_in_order_before_exit() {
    let mut shutdown = ShutdownCoordinator::new();
    let begin = shutdown
        .begin(ShutdownReason::Quit, 1_000, 10_000)
        .expect("begin shutdown");
    let generation = ShellGeneration::new(1);
    assert_eq!(begin, ShutdownEffect::PublishDurableState { generation });
    assert_eq!(shutdown.stage(), ShutdownStage::AwaitingDurableState);

    assert_eq!(
        shutdown.acknowledge(
            generation,
            ShutdownAcknowledgement::DurableStatePublished,
            1_010,
        ),
        Ok(ShutdownEffect::StopWindowMedia { generation })
    );
    assert_eq!(
        shutdown.acknowledge(
            generation,
            ShutdownAcknowledgement::WindowMediaStopped,
            1_020,
        ),
        Ok(ShutdownEffect::FinalizeRecorder { generation })
    );
    assert_eq!(
        shutdown.acknowledge(
            generation,
            ShutdownAcknowledgement::RecorderFinalized,
            1_030,
        ),
        Ok(ShutdownEffect::FlushDiagnostics { generation })
    );
    assert_eq!(
        shutdown.acknowledge(
            generation,
            ShutdownAcknowledgement::DiagnosticsFlushed,
            1_040,
        ),
        Ok(ShutdownEffect::ReadyToExit {
            generation,
            reason: ShutdownReason::Quit,
        })
    );
    assert_eq!(shutdown.stage(), ShutdownStage::ReadyToExit);
}

#[test]
fn stale_duplicate_and_out_of_order_acknowledgements_do_not_advance() {
    let mut shutdown = ShutdownCoordinator::new();
    shutdown
        .begin(ShutdownReason::InstallUpdate, 50, 500)
        .expect("begin shutdown");
    let generation = ShellGeneration::new(1);
    let before = shutdown;

    assert!(matches!(
        shutdown.acknowledge(
            ShellGeneration::INITIAL,
            ShutdownAcknowledgement::DurableStatePublished,
            51,
        ),
        Err(ShutdownError::StaleGeneration { .. })
    ));
    assert_eq!(shutdown, before);
    assert!(matches!(
        shutdown.acknowledge(generation, ShutdownAcknowledgement::RecorderFinalized, 52,),
        Err(ShutdownError::UnexpectedAcknowledgement { .. })
    ));
    assert_eq!(shutdown, before);

    shutdown
        .acknowledge(
            generation,
            ShutdownAcknowledgement::DurableStatePublished,
            53,
        )
        .unwrap();
    let after_first_ack = shutdown;
    assert!(matches!(
        shutdown.acknowledge(
            generation,
            ShutdownAcknowledgement::DurableStatePublished,
            54,
        ),
        Err(ShutdownError::UnexpectedAcknowledgement { .. })
    ));
    assert_eq!(shutdown, after_first_ack);
}

#[test]
fn shutdown_deadlines_and_generation_exhaustion_fail_closed() {
    let mut shutdown = ShutdownCoordinator::new();
    assert!(matches!(
        shutdown.begin(ShutdownReason::Quit, 0, MAX_SHUTDOWN_TIMEOUT_MS + 1),
        Err(ShutdownError::InvalidTimeout { .. })
    ));
    assert_eq!(shutdown.stage(), ShutdownStage::Idle);

    shutdown
        .begin(ShutdownReason::Quit, 100, 25)
        .expect("begin bounded shutdown");
    assert!(matches!(
        shutdown.acknowledge(
            ShellGeneration::new(1),
            ShutdownAcknowledgement::DurableStatePublished,
            126,
        ),
        Err(ShutdownError::DeadlineExceeded { .. })
    ));
    assert_eq!(shutdown.stage(), ShutdownStage::Expired);
    assert!(!shutdown.may_exit());

    let mut exhausted = ShutdownCoordinator::starting_at(ShellGeneration::new(u64::MAX));
    assert_eq!(
        exhausted.begin(ShutdownReason::Quit, 0, 1),
        Err(ShutdownError::GenerationExhausted)
    );
    assert_eq!(exhausted.stage(), ShutdownStage::Idle);
}

#[test]
fn shutdown_rejects_clock_regression_and_only_final_stage_authorizes_exit() {
    let mut shutdown = ShutdownCoordinator::new();
    shutdown
        .begin(ShutdownReason::InstallUpdate, 10_000, 1_000)
        .unwrap();
    assert!(!shutdown.may_exit());
    assert!(matches!(
        shutdown.acknowledge(
            ShellGeneration::new(1),
            ShutdownAcknowledgement::DurableStatePublished,
            9_999,
        ),
        Err(ShutdownError::ClockRegression { .. })
    ));
    assert_eq!(shutdown.stage(), ShutdownStage::AwaitingDurableState);
    assert!(!shutdown.may_exit());
}
