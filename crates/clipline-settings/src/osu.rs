//! osu! API settings persisted in `settings.json`.
//!
//! The OAuth client secret is intentionally not part of this struct. It is
//! stored in Windows Credential Manager under `credential_target`.

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const OSU_CREDENTIAL_PREFIX: &str = "Clipline osu!";
pub const MAX_OSU_CLIENT_ID_DIGITS: usize = 20;
pub const MAX_OSU_USER_BYTES: usize = 256;
pub const MAX_OSU_CONNECTED_USERNAME_BYTES: usize = 256;
pub const MAX_OSU_CREDENTIAL_TARGET_BYTES: usize = 4 * 1024;
pub const MAX_OSU_CREDENTIAL_CLEANUP_TARGETS: usize = 16;
pub const MAX_OSU_PROFILE_BYTES: usize = 64 * 1024;

pub fn osu_credential_target(client_id: &str, user: &str) -> String {
    format!(
        "{OSU_CREDENTIAL_PREFIX}:{}:{}",
        client_id.trim(),
        user.trim()
    )
}

/// Credential target owned by one exact osu! account operation.
///
/// The operation id is generated independently for every write, including by
/// separately constructed frontend adapters. The target grammar deliberately
/// excludes user-controlled client/user text, so a legacy target cannot be
/// crafted to collide with a new candidate.
pub fn osu_credential_target_for_operation(
    generation: OsuAccountGeneration,
    operation_id: &str,
) -> Result<String, String> {
    if operation_id.is_empty()
        || operation_id.len() > 64
        || !operation_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err("osu! credential operation id is invalid".into());
    }
    Ok(format!(
        "{OSU_CREDENTIAL_PREFIX}:generation:{}:operation:{operation_id}",
        generation.get()
    ))
}

pub fn is_osu_credential_target_for_generation(
    target: &str,
    generation: OsuAccountGeneration,
) -> bool {
    let prefix = format!(
        "{OSU_CREDENTIAL_PREFIX}:generation:{}:operation:",
        generation.get()
    );
    let Some(operation_id) = target.strip_prefix(&prefix) else {
        return false;
    };
    !operation_id.is_empty()
        && operation_id.len() <= 64
        && operation_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OsuAccountGeneration(u64);

impl OsuAccountGeneration {
    pub const INITIAL: Self = Self(1);

    pub const fn new(value: u64) -> Result<Self, OsuAccountGenerationError> {
        if value == 0 {
            Err(OsuAccountGenerationError::Zero)
        } else {
            Ok(Self(value))
        }
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub const fn checked_next(self) -> Result<Self, OsuAccountGenerationError> {
        if self.0 == 0 {
            return Err(OsuAccountGenerationError::Zero);
        }
        match self.0.checked_add(1) {
            Some(value) => Ok(Self(value)),
            None => Err(OsuAccountGenerationError::Exhausted),
        }
    }

    const fn validate(self) -> Result<(), OsuAccountGenerationError> {
        if self.0 == 0 {
            Err(OsuAccountGenerationError::Zero)
        } else {
            Ok(())
        }
    }
}

impl Default for OsuAccountGeneration {
    fn default() -> Self {
        Self::INITIAL
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum OsuAccountGenerationError {
    #[error("osu! account generation must not be zero")]
    Zero,
    #[error("osu! account generation is exhausted")]
    Exhausted,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OsuApiSettings {
    #[serde(default)]
    pub account_generation: OsuAccountGeneration,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_target: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credential_cleanup_targets: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_connected_username: Option<String>,
}

impl OsuApiSettings {
    pub fn normalize(&mut self) {
        // Resource bounds are checked before canonicalization. Otherwise a
        // hostile list containing too many duplicate or blank targets could
        // be silently reduced into an accepted durable profile.
        if self.validate_resource_bounds().is_err() {
            return;
        }
        self.client_id = clean_optional(self.client_id.take());
        self.user = clean_optional(self.user.take());
        self.credential_target = clean_optional(self.credential_target.take());
        self.credential_cleanup_targets = std::mem::take(&mut self.credential_cleanup_targets)
            .into_iter()
            .filter_map(|value| clean_optional(Some(value)))
            .collect();
        self.credential_cleanup_targets.sort();
        self.credential_cleanup_targets.dedup();
        self.last_connected_username = clean_optional(self.last_connected_username.take());
    }

    pub fn validate(&self) -> Result<(), String> {
        self.validate_resource_bounds()?;
        self.account_generation
            .validate()
            .map_err(|error| error.to_string())?;
        if let Some(client_id) = self.client_id.as_deref() {
            if client_id.is_empty()
                || !client_id.bytes().all(|byte| byte.is_ascii_digit())
                || client_id.parse::<u64>().is_err()
            {
                return Err("osu! client id must be a number".into());
            }
        }
        if let Some(target) = self.credential_target.as_deref() {
            validate_owned_credential_target("osu! credential target", target)?;
        }
        for target in &self.credential_cleanup_targets {
            validate_owned_credential_target("osu! credential cleanup target", target)?;
        }
        if self.credential_target.as_ref().is_some_and(|active| {
            self.credential_cleanup_targets
                .iter()
                .any(|target| target == active)
        }) {
            return Err("osu! active credential target cannot be scheduled for cleanup".into());
        }
        Ok(())
    }

    fn validate_resource_bounds(&self) -> Result<(), String> {
        let mut aggregate = 0usize;
        account_optional_text(
            &mut aggregate,
            "osu! client id",
            self.client_id.as_deref(),
            MAX_OSU_CLIENT_ID_DIGITS,
        )?;
        account_optional_text(
            &mut aggregate,
            "osu! user",
            self.user.as_deref(),
            MAX_OSU_USER_BYTES,
        )?;
        account_optional_text(
            &mut aggregate,
            "osu! credential target",
            self.credential_target.as_deref(),
            MAX_OSU_CREDENTIAL_TARGET_BYTES,
        )?;
        account_optional_text(
            &mut aggregate,
            "osu! connected username",
            self.last_connected_username.as_deref(),
            MAX_OSU_CONNECTED_USERNAME_BYTES,
        )?;
        if self.credential_cleanup_targets.len() > MAX_OSU_CREDENTIAL_CLEANUP_TARGETS {
            return Err(format!(
                "osu! credential cleanup targets must contain at most {MAX_OSU_CREDENTIAL_CLEANUP_TARGETS} entries"
            ));
        }
        for target in &self.credential_cleanup_targets {
            account_text(
                &mut aggregate,
                "osu! credential cleanup target",
                target,
                MAX_OSU_CREDENTIAL_TARGET_BYTES,
            )?;
        }
        Ok(())
    }
}

fn validate_owned_credential_target(label: &str, target: &str) -> Result<(), String> {
    if !target.starts_with(OSU_CREDENTIAL_PREFIX)
        || target.as_bytes().get(OSU_CREDENTIAL_PREFIX.len()) != Some(&b':')
    {
        return Err(format!(
            "{label} is outside Clipline's osu! credential namespace"
        ));
    }
    Ok(())
}

fn account_optional_text(
    aggregate: &mut usize,
    label: &str,
    value: Option<&str>,
    maximum: usize,
) -> Result<(), String> {
    if let Some(value) = value {
        account_text(aggregate, label, value, maximum)?;
    }
    Ok(())
}

fn account_text(
    aggregate: &mut usize,
    label: &str,
    value: &str,
    maximum: usize,
) -> Result<(), String> {
    if value.len() > maximum {
        return Err(format!(
            "{label} is {} UTF-8 bytes; maximum is {maximum}",
            value.len()
        ));
    }
    *aggregate = aggregate
        .checked_add(value.len())
        .ok_or_else(|| "osu! profile aggregate byte count overflowed".to_string())?;
    if *aggregate > MAX_OSU_PROFILE_BYTES {
        return Err(format!(
            "osu! profile aggregate is {aggregate} UTF-8 bytes; maximum is {MAX_OSU_PROFILE_BYTES}"
        ));
    }
    Ok(())
}

fn clean_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}
