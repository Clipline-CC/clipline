//! osu! API settings persisted in `settings.json`.
//!
//! The OAuth client secret is intentionally not part of this struct. It is
//! stored in Windows Credential Manager under `credential_target`.

use serde::{Deserialize, Serialize};

pub const OSU_CREDENTIAL_PREFIX: &str = "Clipline osu!";

pub fn osu_credential_target(client_id: &str, user: &str) -> String {
    format!(
        "{OSU_CREDENTIAL_PREFIX}:{}:{}",
        client_id.trim(),
        user.trim()
    )
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OsuApiSettings {
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
        if let Some(client_id) = self.client_id.as_deref() {
            if client_id.parse::<u64>().is_err() {
                return Err("osu! client id must be a number".into());
            }
        }
        Ok(())
    }
}

fn clean_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}
