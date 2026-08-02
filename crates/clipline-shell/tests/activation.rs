use clipline_shell::activation::{
    validate_activation_peer, ActivationCommand, ActivationEnvelope, ActivationError,
    ActivationPeer, MAX_ACTIVATION_PAYLOAD_BYTES,
};
use clipline_shell::{ProcessIdentity, ShellCommand};

fn identity(process_id: u32, creation_time: u64) -> ProcessIdentity {
    ProcessIdentity::new(process_id, creation_time).unwrap()
}

#[test]
fn normal_and_autostart_commands_map_to_the_exact_shell_effect() {
    let normal = ActivationEnvelope::new(ActivationCommand::Reveal, identity(41, 100));
    assert_eq!(normal.shell_command(), Some(ShellCommand::Open));
    assert_eq!(
        ActivationEnvelope::decode(&normal.encode().unwrap()).unwrap(),
        normal
    );

    let autostart = ActivationEnvelope::new(ActivationCommand::AutostartNoop, identity(42, 101));
    assert_eq!(autostart.shell_command(), None);
    assert_eq!(
        ActivationEnvelope::decode(&autostart.encode().unwrap()).unwrap(),
        autostart
    );
}

#[test]
fn payload_parser_is_bounded_strict_and_versioned() {
    let oversized = vec![b'x'; MAX_ACTIVATION_PAYLOAD_BYTES + 1];
    assert_eq!(
        ActivationEnvelope::decode(&oversized),
        Err(ActivationError::PayloadTooLarge {
            actual: MAX_ACTIVATION_PAYLOAD_BYTES + 1,
            maximum: MAX_ACTIVATION_PAYLOAD_BYTES,
        })
    );
    assert_eq!(
        ActivationEnvelope::decode(&[0xff]),
        Err(ActivationError::InvalidUtf8)
    );

    for invalid in [
        r#"{"schema":1,"schema":1,"command":"reveal","client":{"process_id":1,"creation_time":2}}"#,
        r#"{"schema":1,"command":"reveal","command":"autostart_noop","client":{"process_id":1,"creation_time":2}}"#,
        r#"{"schema":1,"command":"unsupported","client":{"process_id":1,"creation_time":2}}"#,
        r#"{"schema":1,"command":"reveal","client":{"process_id":1,"creation_time":2},"extra":true}"#,
        r#"{"schema":1,"command":"reveal"}"#,
        "{",
    ] {
        assert!(
            matches!(
                ActivationEnvelope::decode(invalid.as_bytes()),
                Err(ActivationError::InvalidPayload(_))
            ),
            "payload should fail closed: {invalid}"
        );
    }

    let unsupported =
        r#"{"schema":2,"command":"reveal","client":{"process_id":1,"creation_time":2}}"#;
    assert_eq!(
        ActivationEnvelope::decode(unsupported.as_bytes()),
        Err(ActivationError::UnsupportedSchema(2))
    );
}

#[test]
fn peer_authentication_uses_sid_and_exact_process_instance() {
    let expected = ActivationPeer {
        sid: vec![1, 2, 3, 4],
        process: identity(91, 700),
    };
    assert!(validate_activation_peer(&expected, &expected).is_ok());

    let other_user = ActivationPeer {
        sid: vec![1, 2, 3, 5],
        process: expected.process,
    };
    assert_eq!(
        validate_activation_peer(&expected, &other_user),
        Err(ActivationError::PeerSidMismatch)
    );

    let recycled_pid = ActivationPeer {
        sid: expected.sid.clone(),
        process: identity(expected.process.process_id(), 701),
    };
    assert_eq!(
        validate_activation_peer(&expected, &recycled_pid),
        Err(ActivationError::PeerProcessMismatch)
    );
}
