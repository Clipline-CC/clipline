use clipline_shell::{
    LaunchMode, ProcessIdentity, ShellCommand, ShellCounterError, ShellGeneration, ShellLaunch,
    ShellLaunchError, ShellSequence, WindowEffect, WindowEvent, WindowMode, WindowPolicy,
};

#[test]
fn launch_parser_preserves_modes_handoffs_and_application_arguments() {
    let normal = ShellLaunch::parse(["clipline.exe", "--window", "League of Legends"])
        .expect("normal launch");
    assert_eq!(normal.mode(), LaunchMode::Normal);
    assert_eq!(
        normal.application_arguments(),
        ["--window", "League of Legends"]
    );

    let autostart = ShellLaunch::parse(["clipline.exe", "--autostart"]).expect("autostart");
    assert_eq!(autostart.mode(), LaunchMode::Autostart);
    assert!(autostart.application_arguments().is_empty());

    let elevated = ShellLaunch::parse([
        "clipline.exe",
        "--clipline-elevated-after",
        "4242",
        "987654321",
    ])
    .expect("elevation handoff");
    assert_eq!(
        elevated.elevation_parent(),
        Some(ProcessIdentity::new(4242, 987_654_321).unwrap())
    );

    let updated = ShellLaunch::parse(["clipline.exe", "--clipline-updated-after", "88", "123456"])
        .expect("updater handoff");
    assert_eq!(
        updated.updater_parent(),
        Some(ProcessIdentity::new(88, 123_456).unwrap())
    );
}

#[test]
fn shell_identity_counters_fail_closed_at_exhaustion() {
    assert_eq!(
        ShellGeneration::new(u64::MAX).checked_next(),
        Err(ShellCounterError::Exhausted)
    );
    assert_eq!(
        ShellSequence::new(u64::MAX).checked_next(),
        Err(ShellCounterError::Exhausted)
    );
}

#[test]
fn launch_parser_rejects_duplicates_missing_values_and_oversized_input_atomically() {
    assert!(matches!(
        ShellLaunch::parse(["clipline.exe", "--autostart", "--autostart"]),
        Err(ShellLaunchError::DuplicateArgument("--autostart"))
    ));
    assert!(matches!(
        ShellLaunch::parse(["clipline.exe", "--clipline-elevated-after", "4"]),
        Err(ShellLaunchError::MissingValue("--clipline-elevated-after"))
    ));
    assert!(matches!(
        ShellLaunch::parse([
            "clipline.exe",
            "--clipline-elevated-after",
            "not-a-pid",
            "5",
        ]),
        Err(ShellLaunchError::InvalidProcessId { .. })
    ));
    let oversized = "x".repeat(4097);
    assert!(matches!(
        ShellLaunch::parse(["clipline.exe", oversized.as_str()]),
        Err(ShellLaunchError::ArgumentTooLong { .. })
    ));
}

#[test]
fn shell_commands_have_explicit_durability_and_stable_json() {
    for command in [
        ShellCommand::Open,
        ShellCommand::SaveReplay,
        ShellCommand::OpenDiagnostics,
        ShellCommand::Quit,
        ShellCommand::CheckUpdates,
        ShellCommand::InstallUpdate,
    ] {
        let encoded = serde_json::to_string(&command).expect("serialize shell command");
        let decoded: ShellCommand = serde_json::from_str(&encoded).expect("deserialize command");
        assert_eq!(decoded, command);
    }

    assert!(ShellCommand::Open.is_coalescable());
    assert!(ShellCommand::SaveReplay.is_coalescable());
    assert!(!ShellCommand::OpenDiagnostics.is_coalescable());
    assert!(ShellCommand::Quit.is_durable());
    assert!(ShellCommand::InstallUpdate.is_barrier());
    assert_eq!(
        serde_json::to_string(&ShellCommand::SaveReplay).unwrap(),
        r#"{"kind":"save_replay"}"#
    );
}

#[test]
fn window_policy_maps_launch_and_lifecycle_without_framework_types() {
    let (mut normal, effect) = WindowPolicy::for_launch(LaunchMode::Normal);
    assert_eq!(normal.mode(), WindowMode::Foreground);
    assert_eq!(effect, WindowEffect::CreateAndReveal);
    assert_eq!(
        normal.apply(WindowEvent::CloseRequested),
        WindowEffect::DropToTray
    );
    assert_eq!(normal.mode(), WindowMode::Tray);
    assert_eq!(
        normal.apply(WindowEvent::RevealRequested),
        WindowEffect::CreateAndReveal
    );
    assert_eq!(
        normal.apply(WindowEvent::MinimizeRequested),
        WindowEffect::ShowInTaskbar
    );
    assert_eq!(normal.mode(), WindowMode::Taskbar);
    assert_eq!(normal.apply(WindowEvent::QuitRequested), WindowEffect::Quit);
    assert!(normal.is_quitting());

    let (mut autostart, effect) = WindowPolicy::for_launch(LaunchMode::Autostart);
    assert_eq!(autostart.mode(), WindowMode::Tray);
    assert_eq!(effect, WindowEffect::KeepTrayOnly);
    assert_eq!(
        autostart.apply(WindowEvent::RevealRequested),
        WindowEffect::CreateAndReveal
    );
}
