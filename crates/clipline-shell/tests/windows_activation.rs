#![cfg(windows)]

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clipline_shell::activation::{ActivationCommand, MAX_ACTIVATION_PAYLOAD_BYTES};
use clipline_shell::windows::activation::{
    acquire_or_activate, instance_names, ActivationAcknowledgement, WindowsInstanceRole,
};
use clipline_shell::{shell_command_channel, ShellCommand};

fn product_identity(case: &str) -> String {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    format!("io.clipline.test.{case}.{}.{nonce}", std::process::id())
}

#[test]
fn names_are_product_and_sid_scoped_without_username_text() {
    let first = instance_names("io.clipline.app", &[1, 2, 0xfe]).unwrap();
    let second = instance_names("io.clipline.app", &[1, 2, 0xff]).unwrap();
    assert_eq!(first.mutex, r"Global\io.clipline.app.0102FE.instance");
    assert_eq!(first.pipe, r"\\.\pipe\io.clipline.app.0102FE.activation");
    assert_ne!(first, second);
    assert!(!first.mutex.to_ascii_lowercase().contains("username"));
    assert!(instance_names("io clipline", &[1]).is_err());
}

#[test]
fn primary_queues_reveal_and_acknowledges_secondary_autostart() {
    let product = product_identity("delivery");
    let (sender, receiver) = shell_command_channel();
    let primary = match acquire_or_activate(&product, ActivationCommand::Reveal, sender.clone())
        .expect("acquire primary")
    {
        WindowsInstanceRole::Primary(primary) => primary,
        WindowsInstanceRole::Secondary(_) => panic!("unique identity must become primary"),
    };

    let normal = acquire_or_activate(&product, ActivationCommand::Reveal, sender.clone())
        .expect("activate primary");
    assert!(matches!(
        normal,
        WindowsInstanceRole::Secondary(ActivationAcknowledgement::RevealQueued)
    ));
    let queued = receiver
        .wait_recv(Duration::from_secs(1))
        .expect("command producer remains connected")
        .expect("pre-UI activation remains queued");
    assert_eq!(queued.command, ShellCommand::Open);

    let autostart = acquire_or_activate(&product, ActivationCommand::AutostartNoop, sender)
        .expect("acknowledge autostart duplicate");
    assert!(matches!(
        autostart,
        WindowsInstanceRole::Secondary(ActivationAcknowledgement::AutostartAcknowledged)
    ));
    assert!(receiver.try_recv().is_none());
    assert_eq!(primary.snapshot().accepted_activations, 2);

    primary.shutdown().expect("stop and join listener");
}

#[test]
fn malformed_oversized_incomplete_and_timed_out_frames_are_rejected() {
    let product = product_identity("reject");
    let (sender, receiver) = shell_command_channel();
    let primary = match acquire_or_activate(&product, ActivationCommand::Reveal, sender)
        .expect("acquire primary")
    {
        WindowsInstanceRole::Primary(primary) => primary,
        WindowsInstanceRole::Secondary(_) => panic!("unique identity must become primary"),
    };

    let invalid_utf8 = [1_u8, 0, 0, 0, 0xff];
    assert!(!primary
        .send_raw_frame_for_test(&invalid_utf8)
        .expect("receive invalid UTF-8 rejection"));

    let oversized = u32::try_from(MAX_ACTIVATION_PAYLOAD_BYTES + 1)
        .unwrap()
        .to_le_bytes();
    assert!(!primary
        .send_raw_frame_for_test(&oversized)
        .expect("receive oversize rejection"));

    let duplicate = br#"{"schema":1,"schema":1,"command":"reveal","client":{"process_id":1,"creation_time":2}}"#;
    let mut duplicate_frame = Vec::from(u32::try_from(duplicate.len()).unwrap().to_le_bytes());
    duplicate_frame.extend_from_slice(duplicate);
    assert!(!primary
        .send_raw_frame_for_test(&duplicate_frame)
        .expect("receive duplicate-field rejection"));

    primary
        .send_incomplete_frame_for_test(&[1, 0])
        .expect("send incomplete prefix");
    let incomplete_deadline = Instant::now() + Duration::from_secs(1);
    while primary.snapshot().rejected_activations < 4 && Instant::now() < incomplete_deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(primary.snapshot().rejected_activations, 4);
    primary
        .stall_connection_for_test()
        .expect("stall connection through deadline");
    let deadline = Instant::now() + Duration::from_secs(1);
    while primary.snapshot().rejected_activations < 5 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(primary.snapshot().rejected_activations, 5);
    assert!(receiver.is_empty());
    primary.shutdown().expect("stop and join listener");
}

#[test]
fn listener_shutdown_releases_ownership_for_a_new_primary() {
    let product = product_identity("shutdown");
    let (sender, _receiver) = shell_command_channel();
    let primary = match acquire_or_activate(&product, ActivationCommand::Reveal, sender.clone())
        .expect("acquire first primary")
    {
        WindowsInstanceRole::Primary(primary) => primary,
        WindowsInstanceRole::Secondary(_) => panic!("unique identity must become primary"),
    };
    assert!(primary.snapshot().listener_alive);
    primary.shutdown().expect("stop first primary");

    let replacement = match acquire_or_activate(&product, ActivationCommand::Reveal, sender)
        .expect("acquire replacement primary")
    {
        WindowsInstanceRole::Primary(primary) => primary,
        WindowsInstanceRole::Secondary(_) => panic!("released identity must become primary"),
    };
    assert!(replacement.snapshot().listener_alive);
    replacement.shutdown().expect("stop replacement primary");
}
