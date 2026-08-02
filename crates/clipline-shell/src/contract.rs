use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MAX_LAUNCH_ARGUMENTS: usize = 64;
pub const MAX_LAUNCH_ARGUMENT_BYTES: usize = 4 * 1024;
pub const MAX_LAUNCH_TOTAL_BYTES: usize = 16 * 1024;

const AUTOSTART_ARGUMENT: &str = "--autostart";
const ELEVATED_AFTER_ARGUMENT: &str = "--clipline-elevated-after";
const UPDATED_AFTER_ARGUMENT: &str = "--clipline-updated-after";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ShellCounterError {
    #[error("shell counter is exhausted")]
    Exhausted,
}

macro_rules! checked_counter {
    ($name:ident) => {
        #[derive(
            Debug,
            Default,
            Clone,
            Copy,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            Serialize,
            Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(u64);

        impl $name {
            pub const INITIAL: Self = Self(0);

            #[must_use]
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            #[must_use]
            pub const fn get(self) -> u64 {
                self.0
            }

            pub const fn checked_next(self) -> Result<Self, ShellCounterError> {
                match self.0.checked_add(1) {
                    Some(value) => Ok(Self(value)),
                    None => Err(ShellCounterError::Exhausted),
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

checked_counter!(ShellGeneration);
checked_counter!(ShellSequence);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessIdentity {
    process_id: u32,
    creation_time: u64,
}

impl ProcessIdentity {
    pub fn new(process_id: u32, creation_time: u64) -> Result<Self, ShellLaunchError> {
        if process_id == 0 {
            return Err(ShellLaunchError::InvalidProcessId {
                value: process_id.to_string(),
            });
        }
        if creation_time == 0 {
            return Err(ShellLaunchError::InvalidCreationTime {
                value: creation_time.to_string(),
            });
        }
        Ok(Self {
            process_id,
            creation_time,
        })
    }

    #[must_use]
    pub const fn process_id(self) -> u32 {
        self.process_id
    }

    #[must_use]
    pub const fn creation_time(self) -> u64 {
        self.creation_time
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchMode {
    Normal,
    Autostart,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellLaunch {
    mode: LaunchMode,
    elevation_parent: Option<ProcessIdentity>,
    updater_parent: Option<ProcessIdentity>,
    application_arguments: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ShellLaunchError {
    #[error("launch arguments must include the executable")]
    MissingExecutable,
    #[error("too many launch arguments: {count}; maximum is {maximum}")]
    TooManyArguments { count: usize, maximum: usize },
    #[error("launch argument {index} is {bytes} bytes; maximum is {maximum}")]
    ArgumentTooLong {
        index: usize,
        bytes: usize,
        maximum: usize,
    },
    #[error("launch arguments total {bytes} bytes; maximum is {maximum}")]
    ArgumentsTooLarge { bytes: usize, maximum: usize },
    #[error("duplicate launch argument {0}")]
    DuplicateArgument(&'static str),
    #[error("launch argument {0} is missing a value")]
    MissingValue(&'static str),
    #[error("invalid process id {value}")]
    InvalidProcessId { value: String },
    #[error("invalid process creation time {value}")]
    InvalidCreationTime { value: String },
}

impl ShellLaunch {
    pub fn parse<I, S>(arguments: I) -> Result<Self, ShellLaunchError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let arguments = arguments
            .into_iter()
            .map(|argument| argument.as_ref().to_owned())
            .collect::<Vec<_>>();
        if arguments.is_empty() {
            return Err(ShellLaunchError::MissingExecutable);
        }
        if arguments.len() > MAX_LAUNCH_ARGUMENTS {
            return Err(ShellLaunchError::TooManyArguments {
                count: arguments.len(),
                maximum: MAX_LAUNCH_ARGUMENTS,
            });
        }
        let mut total_bytes = 0_usize;
        for (index, argument) in arguments.iter().enumerate() {
            let bytes = argument.len();
            if bytes > MAX_LAUNCH_ARGUMENT_BYTES {
                return Err(ShellLaunchError::ArgumentTooLong {
                    index,
                    bytes,
                    maximum: MAX_LAUNCH_ARGUMENT_BYTES,
                });
            }
            total_bytes =
                total_bytes
                    .checked_add(bytes)
                    .ok_or(ShellLaunchError::ArgumentsTooLarge {
                        bytes: usize::MAX,
                        maximum: MAX_LAUNCH_TOTAL_BYTES,
                    })?;
            if total_bytes > MAX_LAUNCH_TOTAL_BYTES {
                return Err(ShellLaunchError::ArgumentsTooLarge {
                    bytes: total_bytes,
                    maximum: MAX_LAUNCH_TOTAL_BYTES,
                });
            }
        }

        let mut mode = LaunchMode::Normal;
        let mut saw_autostart = false;
        let mut elevation_parent = None;
        let mut updater_parent = None;
        let mut application_arguments = Vec::new();
        let mut index = 1;
        while index < arguments.len() {
            match arguments[index].as_str() {
                AUTOSTART_ARGUMENT => {
                    if saw_autostart {
                        return Err(ShellLaunchError::DuplicateArgument(AUTOSTART_ARGUMENT));
                    }
                    saw_autostart = true;
                    mode = LaunchMode::Autostart;
                    index += 1;
                }
                ELEVATED_AFTER_ARGUMENT => {
                    if elevation_parent.is_some() {
                        return Err(ShellLaunchError::DuplicateArgument(ELEVATED_AFTER_ARGUMENT));
                    }
                    elevation_parent = Some(parse_process_identity(
                        &arguments,
                        index,
                        ELEVATED_AFTER_ARGUMENT,
                    )?);
                    index += 3;
                }
                UPDATED_AFTER_ARGUMENT => {
                    if updater_parent.is_some() {
                        return Err(ShellLaunchError::DuplicateArgument(UPDATED_AFTER_ARGUMENT));
                    }
                    updater_parent = Some(parse_process_identity(
                        &arguments,
                        index,
                        UPDATED_AFTER_ARGUMENT,
                    )?);
                    index += 3;
                }
                _ => {
                    application_arguments.push(arguments[index].clone());
                    index += 1;
                }
            }
        }

        Ok(Self {
            mode,
            elevation_parent,
            updater_parent,
            application_arguments,
        })
    }

    #[must_use]
    pub const fn mode(&self) -> LaunchMode {
        self.mode
    }

    #[must_use]
    pub const fn elevation_parent(&self) -> Option<ProcessIdentity> {
        self.elevation_parent
    }

    #[must_use]
    pub const fn updater_parent(&self) -> Option<ProcessIdentity> {
        self.updater_parent
    }

    #[must_use]
    pub fn application_arguments(&self) -> &[String] {
        &self.application_arguments
    }
}

fn parse_process_identity(
    arguments: &[String],
    flag_index: usize,
    flag: &'static str,
) -> Result<ProcessIdentity, ShellLaunchError> {
    let process_id = arguments
        .get(flag_index + 1)
        .ok_or(ShellLaunchError::MissingValue(flag))?;
    let creation_time = arguments
        .get(flag_index + 2)
        .ok_or(ShellLaunchError::MissingValue(flag))?;
    let process_id = process_id
        .parse::<u32>()
        .map_err(|_| ShellLaunchError::InvalidProcessId {
            value: process_id.clone(),
        })?;
    let creation_time =
        creation_time
            .parse::<u64>()
            .map_err(|_| ShellLaunchError::InvalidCreationTime {
                value: creation_time.clone(),
            })?;
    ProcessIdentity::new(process_id, creation_time)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ShellCommand {
    Open,
    SaveReplay,
    OpenDiagnostics,
    Quit,
    CheckUpdates,
    InstallUpdate,
}

impl ShellCommand {
    #[must_use]
    pub const fn is_coalescable(self) -> bool {
        matches!(self, Self::Open | Self::SaveReplay)
    }

    #[must_use]
    pub const fn is_durable(self) -> bool {
        matches!(
            self,
            Self::OpenDiagnostics | Self::Quit | Self::InstallUpdate
        )
    }

    #[must_use]
    pub const fn is_barrier(self) -> bool {
        matches!(self, Self::Quit | Self::InstallUpdate)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowMode {
    Foreground,
    Tray,
    Taskbar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowEvent {
    CloseRequested,
    MinimizeRequested,
    RevealRequested,
    TaskbarRequested,
    QuitRequested,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowEffect {
    KeepTrayOnly,
    CreateAndReveal,
    DropToTray,
    ShowInTaskbar,
    Quit,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowPolicy {
    mode: WindowMode,
    quitting: bool,
}

impl WindowPolicy {
    #[must_use]
    pub const fn for_launch(mode: LaunchMode) -> (Self, WindowEffect) {
        match mode {
            LaunchMode::Normal => (
                Self {
                    mode: WindowMode::Foreground,
                    quitting: false,
                },
                WindowEffect::CreateAndReveal,
            ),
            LaunchMode::Autostart => (
                Self {
                    mode: WindowMode::Tray,
                    quitting: false,
                },
                WindowEffect::KeepTrayOnly,
            ),
        }
    }

    #[must_use]
    pub const fn mode(self) -> WindowMode {
        self.mode
    }

    #[must_use]
    pub const fn is_quitting(self) -> bool {
        self.quitting
    }

    pub fn apply(&mut self, event: WindowEvent) -> WindowEffect {
        if self.quitting {
            return WindowEffect::None;
        }
        match event {
            WindowEvent::CloseRequested => {
                self.mode = WindowMode::Tray;
                WindowEffect::DropToTray
            }
            WindowEvent::MinimizeRequested | WindowEvent::TaskbarRequested => {
                self.mode = WindowMode::Taskbar;
                WindowEffect::ShowInTaskbar
            }
            WindowEvent::RevealRequested => {
                self.mode = WindowMode::Foreground;
                WindowEffect::CreateAndReveal
            }
            WindowEvent::QuitRequested => {
                self.quitting = true;
                WindowEffect::Quit
            }
        }
    }
}
