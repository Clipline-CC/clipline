use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum GenerationError {
    #[error("generation is exhausted")]
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

            pub const fn checked_next(self) -> Result<Self, GenerationError> {
                match self.0.checked_add(1) {
                    Some(value) => Ok(Self(value)),
                    None => Err(GenerationError::Exhausted),
                }
            }
        }
    };
}

checked_counter!(Generation);
checked_counter!(Revision);

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowLifecycleMode {
    Foreground,
    #[default]
    Tray,
    Taskbar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowLifecycleSnapshot {
    pub revision: Revision,
    pub mode: WindowLifecycleMode,
    pub backgrounded: bool,
}

impl WindowLifecycleSnapshot {
    #[must_use]
    pub const fn new(revision: Revision, mode: WindowLifecycleMode) -> Self {
        Self {
            revision,
            mode,
            backgrounded: !matches!(mode, WindowLifecycleMode::Foreground),
        }
    }
}

impl Default for WindowLifecycleSnapshot {
    fn default() -> Self {
        Self::new(Revision::INITIAL, WindowLifecycleMode::Tray)
    }
}
