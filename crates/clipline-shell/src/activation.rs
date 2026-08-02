use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{ProcessIdentity, ShellCommand};

pub const ACTIVATION_SCHEMA: u16 = 1;
pub const MAX_ACTIVATION_PAYLOAD_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivationCommand {
    Reveal,
    AutostartNoop,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivationEnvelope {
    schema: u16,
    command: ActivationCommand,
    client: ProcessIdentity,
}

impl ActivationEnvelope {
    #[must_use]
    pub const fn new(command: ActivationCommand, client: ProcessIdentity) -> Self {
        Self {
            schema: ACTIVATION_SCHEMA,
            command,
            client,
        }
    }

    #[must_use]
    pub const fn command(&self) -> ActivationCommand {
        self.command
    }

    #[must_use]
    pub const fn client(&self) -> ProcessIdentity {
        self.client
    }

    #[must_use]
    pub const fn shell_command(&self) -> Option<ShellCommand> {
        match self.command {
            ActivationCommand::Reveal => Some(ShellCommand::Open),
            ActivationCommand::AutostartNoop => None,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, ActivationError> {
        let payload = serde_json::to_vec(self)
            .map_err(|error| ActivationError::InvalidPayload(error.to_string()))?;
        validate_payload_size(payload.len())?;
        Ok(payload)
    }

    pub fn decode(payload: &[u8]) -> Result<Self, ActivationError> {
        validate_payload_size(payload.len())?;
        let payload = std::str::from_utf8(payload).map_err(|_| ActivationError::InvalidUtf8)?;
        let envelope: Self = serde_json::from_str(payload)
            .map_err(|error| ActivationError::InvalidPayload(error.to_string()))?;
        if envelope.schema != ACTIVATION_SCHEMA {
            return Err(ActivationError::UnsupportedSchema(envelope.schema));
        }
        Ok(envelope)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationPeer {
    pub sid: Vec<u8>,
    pub process: ProcessIdentity,
}

pub fn validate_activation_peer(
    expected: &ActivationPeer,
    actual: &ActivationPeer,
) -> Result<(), ActivationError> {
    if expected.sid != actual.sid {
        return Err(ActivationError::PeerSidMismatch);
    }
    if expected.process != actual.process {
        return Err(ActivationError::PeerProcessMismatch);
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ActivationError {
    #[error("activation payload is {actual} bytes; maximum is {maximum}")]
    PayloadTooLarge { actual: usize, maximum: usize },
    #[error("activation payload is not valid UTF-8")]
    InvalidUtf8,
    #[error("activation payload is invalid: {0}")]
    InvalidPayload(String),
    #[error("unsupported activation schema {0}")]
    UnsupportedSchema(u16),
    #[error("activation peer belongs to a different user SID")]
    PeerSidMismatch,
    #[error("activation peer process identity does not match the connected process")]
    PeerProcessMismatch,
}

fn validate_payload_size(actual: usize) -> Result<(), ActivationError> {
    if actual > MAX_ACTIVATION_PAYLOAD_BYTES {
        Err(ActivationError::PayloadTooLarge {
            actual,
            maximum: MAX_ACTIVATION_PAYLOAD_BYTES,
        })
    } else {
        Ok(())
    }
}
