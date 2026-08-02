//! Framework-neutral desktop shell contract for Clipline.

pub mod activation;
mod channel;
mod contract;
pub mod hotkey;
mod shutdown;

#[cfg(windows)]
pub mod windows;

pub use channel::{
    shell_command_channel, shell_command_channel_starting_at, SequencedShellCommand,
    ShellCommandPublishOutcome, ShellCommandReceiveError, ShellCommandReceiver,
    ShellCommandSendError, ShellCommandSender, SHELL_COMMAND_CAPACITY,
};
pub use contract::{
    LaunchMode, ProcessIdentity, ShellCommand, ShellCounterError, ShellGeneration, ShellLaunch,
    ShellLaunchError, ShellSequence, WindowEffect, WindowEvent, WindowMode, WindowPolicy,
    MAX_LAUNCH_ARGUMENTS, MAX_LAUNCH_ARGUMENT_BYTES, MAX_LAUNCH_TOTAL_BYTES,
};
pub use shutdown::{
    ShutdownAcknowledgement, ShutdownCoordinator, ShutdownEffect, ShutdownError, ShutdownReason,
    ShutdownStage, MAX_SHUTDOWN_TIMEOUT_MS,
};
