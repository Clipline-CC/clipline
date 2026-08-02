use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use thiserror::Error;

use crate::ShellGeneration;

pub const MAX_SHUTDOWN_TIMEOUT_MS: u64 = 30_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShutdownReason {
    Quit,
    InstallUpdate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ShutdownOwnershipError {
    #[error("{active:?} already owns process shutdown; cannot start {requested:?}")]
    Busy {
        active: ShutdownReason,
        requested: ShutdownReason,
    },
    #[error("process shutdown ownership is unavailable")]
    Unavailable,
}

/// Process-wide ownership for the one path allowed to authorize application exit.
#[derive(Default)]
pub struct ShutdownGate {
    owner: Mutex<Option<ShutdownReason>>,
}

impl ShutdownGate {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            owner: Mutex::new(None),
        }
    }

    pub fn begin(
        &self,
        requested: ShutdownReason,
    ) -> Result<ShutdownLease<'_>, ShutdownOwnershipError> {
        let mut owner = self
            .owner
            .lock()
            .map_err(|_| ShutdownOwnershipError::Unavailable)?;
        if let Some(active) = *owner {
            return Err(ShutdownOwnershipError::Busy { active, requested });
        }
        *owner = Some(requested);
        Ok(ShutdownLease {
            gate: self,
            reason: requested,
            release_on_drop: true,
        })
    }

    #[must_use]
    pub fn owner(&self) -> Option<ShutdownReason> {
        match self.owner.lock() {
            Ok(owner) => *owner,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }
}

pub struct ShutdownLease<'a> {
    gate: &'a ShutdownGate,
    reason: ShutdownReason,
    release_on_drop: bool,
}

impl ShutdownLease<'_> {
    /// Keep shutdown ownership latched because application exit is committed.
    pub fn commit_exit(mut self) {
        self.release_on_drop = false;
    }
}

impl Drop for ShutdownLease<'_> {
    fn drop(&mut self) {
        if !self.release_on_drop {
            return;
        }
        let mut owner = match self.gate.owner.lock() {
            Ok(owner) => owner,
            Err(poisoned) => poisoned.into_inner(),
        };
        if *owner == Some(self.reason) {
            *owner = None;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownStage {
    Idle,
    AwaitingDurableState,
    AwaitingWindowMedia,
    AwaitingRecorder,
    AwaitingDiagnostics,
    ReadyToExit,
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownAcknowledgement {
    DurableStatePublished,
    WindowMediaStopped,
    RecorderFinalized,
    DiagnosticsFlushed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownEffect {
    PublishDurableState {
        generation: ShellGeneration,
    },
    StopWindowMedia {
        generation: ShellGeneration,
    },
    FinalizeRecorder {
        generation: ShellGeneration,
    },
    FlushDiagnostics {
        generation: ShellGeneration,
    },
    ReadyToExit {
        generation: ShellGeneration,
        reason: ShutdownReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ShutdownError {
    #[error("shutdown timeout {timeout_ms} ms is outside 1..={maximum_ms} ms")]
    InvalidTimeout { timeout_ms: u64, maximum_ms: u64 },
    #[error("shutdown deadline overflowed")]
    DeadlineOverflow,
    #[error("shutdown generation is exhausted")]
    GenerationExhausted,
    #[error("shutdown is already in progress at {stage:?}")]
    AlreadyInProgress { stage: ShutdownStage },
    #[error("shutdown is not in progress")]
    NotInProgress,
    #[error("shutdown acknowledgement generation {received} is stale; current is {current}")]
    StaleGeneration {
        current: ShellGeneration,
        received: ShellGeneration,
    },
    #[error("shutdown clock regressed from {previous_ms} ms to {received_ms} ms")]
    ClockRegression { previous_ms: u64, received_ms: u64 },
    #[error("shutdown deadline {deadline_ms} ms exceeded at {received_ms} ms")]
    DeadlineExceeded { deadline_ms: u64, received_ms: u64 },
    #[error("expected shutdown acknowledgement {expected:?}, received {received:?}")]
    UnexpectedAcknowledgement {
        expected: ShutdownAcknowledgement,
        received: ShutdownAcknowledgement,
    },
    #[error("shutdown is already ready to exit")]
    AlreadyComplete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShutdownCoordinator {
    generation: ShellGeneration,
    reason: Option<ShutdownReason>,
    stage: ShutdownStage,
    deadline_ms: u64,
    last_observed_ms: u64,
}

impl Default for ShutdownCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl ShutdownCoordinator {
    #[must_use]
    pub const fn new() -> Self {
        Self::starting_at(ShellGeneration::INITIAL)
    }

    /// Constructs an idle coordinator at an explicit generation.
    ///
    /// Production callers should use [`Self::new`]. This seam makes identity
    /// exhaustion deterministic in tests.
    #[doc(hidden)]
    #[must_use]
    pub const fn starting_at(generation: ShellGeneration) -> Self {
        Self {
            generation,
            reason: None,
            stage: ShutdownStage::Idle,
            deadline_ms: 0,
            last_observed_ms: 0,
        }
    }

    pub fn begin(
        &mut self,
        reason: ShutdownReason,
        now_ms: u64,
        timeout_ms: u64,
    ) -> Result<ShutdownEffect, ShutdownError> {
        if !(1..=MAX_SHUTDOWN_TIMEOUT_MS).contains(&timeout_ms) {
            return Err(ShutdownError::InvalidTimeout {
                timeout_ms,
                maximum_ms: MAX_SHUTDOWN_TIMEOUT_MS,
            });
        }
        if self.stage != ShutdownStage::Idle {
            return Err(ShutdownError::AlreadyInProgress { stage: self.stage });
        }
        let generation = self
            .generation
            .checked_next()
            .map_err(|_| ShutdownError::GenerationExhausted)?;
        let deadline_ms = now_ms
            .checked_add(timeout_ms)
            .ok_or(ShutdownError::DeadlineOverflow)?;

        self.generation = generation;
        self.reason = Some(reason);
        self.stage = ShutdownStage::AwaitingDurableState;
        self.deadline_ms = deadline_ms;
        self.last_observed_ms = now_ms;
        Ok(ShutdownEffect::PublishDurableState { generation })
    }

    pub fn acknowledge(
        &mut self,
        generation: ShellGeneration,
        acknowledgement: ShutdownAcknowledgement,
        now_ms: u64,
    ) -> Result<ShutdownEffect, ShutdownError> {
        if self.stage == ShutdownStage::Idle {
            return Err(ShutdownError::NotInProgress);
        }
        if generation != self.generation {
            return Err(ShutdownError::StaleGeneration {
                current: self.generation,
                received: generation,
            });
        }
        if now_ms < self.last_observed_ms {
            return Err(ShutdownError::ClockRegression {
                previous_ms: self.last_observed_ms,
                received_ms: now_ms,
            });
        }
        if now_ms > self.deadline_ms || self.stage == ShutdownStage::Expired {
            self.stage = ShutdownStage::Expired;
            return Err(ShutdownError::DeadlineExceeded {
                deadline_ms: self.deadline_ms,
                received_ms: now_ms,
            });
        }

        let (expected, next_stage, effect) = match self.stage {
            ShutdownStage::AwaitingDurableState => (
                ShutdownAcknowledgement::DurableStatePublished,
                ShutdownStage::AwaitingWindowMedia,
                ShutdownEffect::StopWindowMedia { generation },
            ),
            ShutdownStage::AwaitingWindowMedia => (
                ShutdownAcknowledgement::WindowMediaStopped,
                ShutdownStage::AwaitingRecorder,
                ShutdownEffect::FinalizeRecorder { generation },
            ),
            ShutdownStage::AwaitingRecorder => (
                ShutdownAcknowledgement::RecorderFinalized,
                ShutdownStage::AwaitingDiagnostics,
                ShutdownEffect::FlushDiagnostics { generation },
            ),
            ShutdownStage::AwaitingDiagnostics => (
                ShutdownAcknowledgement::DiagnosticsFlushed,
                ShutdownStage::ReadyToExit,
                ShutdownEffect::ReadyToExit {
                    generation,
                    reason: self.reason.ok_or(ShutdownError::NotInProgress)?,
                },
            ),
            ShutdownStage::ReadyToExit => return Err(ShutdownError::AlreadyComplete),
            ShutdownStage::Idle | ShutdownStage::Expired => {
                return Err(ShutdownError::NotInProgress)
            }
        };
        if acknowledgement != expected {
            return Err(ShutdownError::UnexpectedAcknowledgement {
                expected,
                received: acknowledgement,
            });
        }

        self.stage = next_stage;
        self.last_observed_ms = now_ms;
        Ok(effect)
    }

    #[must_use]
    pub const fn stage(self) -> ShutdownStage {
        self.stage
    }

    #[must_use]
    pub const fn may_exit(self) -> bool {
        matches!(self.stage, ShutdownStage::ReadyToExit)
    }
}
