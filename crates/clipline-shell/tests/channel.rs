use clipline_shell::{
    shell_command_channel, shell_command_channel_starting_at, ShellCommand,
    ShellCommandPublishOutcome, ShellCommandSendError, ShellSequence, SHELL_COMMAND_CAPACITY,
};

#[test]
fn port_reserves_one_slot_for_quit_and_never_blocks_producers() {
    let (sender, receiver) = shell_command_channel();
    for _ in 0..(SHELL_COMMAND_CAPACITY - 1) {
        assert_eq!(
            sender.try_send(ShellCommand::OpenDiagnostics),
            Ok(ShellCommandPublishOutcome::Queued)
        );
    }
    assert_eq!(
        sender.try_send(ShellCommand::CheckUpdates),
        Err(ShellCommandSendError::Full {
            capacity: SHELL_COMMAND_CAPACITY
        })
    );
    assert_eq!(
        sender.try_send(ShellCommand::Quit),
        Ok(ShellCommandPublishOutcome::Queued)
    );
    assert_eq!(receiver.len(), SHELL_COMMAND_CAPACITY);
    assert_eq!(
        sender.try_send(ShellCommand::Quit),
        Ok(ShellCommandPublishOutcome::AlreadyQueued)
    );
}

#[test]
fn coalescing_stays_inside_barriers_and_sequences_follow_delivery_order() {
    let (sender, receiver) = shell_command_channel();
    assert_eq!(
        sender.try_send(ShellCommand::Open),
        Ok(ShellCommandPublishOutcome::Queued)
    );
    assert_eq!(
        sender.try_send(ShellCommand::SaveReplay),
        Ok(ShellCommandPublishOutcome::Queued)
    );
    assert_eq!(
        sender.try_send(ShellCommand::Open),
        Ok(ShellCommandPublishOutcome::Replaced)
    );
    assert_eq!(
        sender.try_send(ShellCommand::InstallUpdate),
        Ok(ShellCommandPublishOutcome::Queued)
    );
    assert_eq!(
        sender.try_send(ShellCommand::Open),
        Ok(ShellCommandPublishOutcome::Queued)
    );

    let updates = (0..4)
        .map(|_| receiver.try_recv().expect("queued command"))
        .collect::<Vec<_>>();
    assert_eq!(
        updates
            .iter()
            .map(|update| update.command)
            .collect::<Vec<_>>(),
        [
            ShellCommand::SaveReplay,
            ShellCommand::Open,
            ShellCommand::InstallUpdate,
            ShellCommand::Open,
        ]
    );
    assert_eq!(
        updates
            .iter()
            .map(|update| update.sequence.get())
            .collect::<Vec<_>>(),
        [2, 3, 4, 5]
    );
}

#[test]
fn port_reports_disconnect_and_sequence_exhaustion_without_mutation() {
    let (sender, receiver) = shell_command_channel();
    drop(receiver);
    assert_eq!(
        sender.try_send(ShellCommand::Open),
        Err(ShellCommandSendError::Disconnected)
    );

    let (sender, receiver) = shell_command_channel_starting_at(ShellSequence::new(u64::MAX));
    assert_eq!(
        sender.try_send(ShellCommand::Open),
        Err(ShellCommandSendError::SequenceExhausted)
    );
    assert!(receiver.is_empty());
}
