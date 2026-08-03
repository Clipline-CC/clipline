//! Whole-settings adapters for the framework-neutral Cloud account service.

use std::collections::btree_map::Entry;
use std::collections::BTreeMap;

use clipline_settings::{
    CloudProfileCas, SettingsSnapshot, SettingsStore, SettingsTransactionError,
};

use crate::{
    account_key, CloudAccountFields, CloudAccountGeneration, CloudAccountKey, CloudAccountSnapshot,
    MAX_CATALOG_STRING_BYTES, MAX_CLOUD_INDEX_ROWS,
};

use super::ports::{CloudAccountPort, CloudProfilePatch, PortError};
use super::CloudServiceAccount;

/// Convert one exact durable settings snapshot into the neutral Cloud account
/// value consumed by catalog/profile services.
///
/// Server `client_clip_id` is the primary local-path key because Cloud list
/// responses expose that identity as `local_clip_id`. Legacy records without
/// it fall back to their durable local id. Conflicting paths for one client id
/// fail closed instead of depending on map iteration order.
pub fn cloud_service_account_from_snapshot(
    snapshot: &SettingsSnapshot,
) -> Result<CloudServiceAccount, PortError> {
    let cloud = &snapshot.document.cloud;
    if cloud.uploads.len() > MAX_CLOUD_INDEX_ROWS {
        return Err(PortError::new(format!(
            "cloud settings contain {} upload records; maximum is {MAX_CLOUD_INDEX_ROWS}",
            cloud.uploads.len()
        )));
    }
    if !cloud.host_url.is_empty() {
        validate_catalog_text("cloud host URL", &cloud.host_url)?;
    }
    validate_optional_catalog_text("cloud public URL", cloud.public_url.as_deref())?;
    validate_optional_catalog_text("cloud username", cloud.connected_username.as_deref())?;
    validate_optional_catalog_text(
        "cloud display name",
        cloud.connected_display_name.as_deref(),
    )?;
    validate_optional_catalog_text("cloud user id", cloud.connected_user_id.as_deref())?;
    validate_optional_catalog_text(
        "cloud credential target",
        cloud.credential_target.as_deref(),
    )?;
    validate_catalog_text("cloud default visibility", &cloud.default_visibility)?;
    // Preflight every borrowed string before cloning any upload identity or
    // path. The borrowed map also detects client-id collisions with memory
    // bounded by the already-checked row cap.
    let mut checked_paths = BTreeMap::new();
    for record in cloud.uploads.values() {
        let clip_id = record
            .client_clip_id
            .as_deref()
            .unwrap_or(record.local_clip_id.as_str());
        validate_catalog_text("cloud client clip id", clip_id)?;
        validate_catalog_text("cloud local path", &record.path)?;
        match checked_paths.entry(clip_id) {
            Entry::Vacant(entry) => {
                entry.insert(record.path.as_str());
            }
            Entry::Occupied(entry) if entry.get() == &record.path.as_str() => {}
            Entry::Occupied(_) => {
                return Err(PortError::new(format!(
                    "cloud client clip id {clip_id:?} maps to multiple local paths"
                )));
            }
        }
    }
    let local_paths_by_clip_id = checked_paths
        .into_iter()
        .map(|(clip_id, path)| (clip_id.to_owned(), path.to_owned()))
        .collect();
    let account_key = account_key(&CloudAccountFields {
        host_url: cloud.host_url.clone(),
        connected_user_id: cloud.connected_user_id.clone().unwrap_or_default(),
        credential_target: cloud.credential_target.clone().unwrap_or_default(),
    })
    .map_err(|error| PortError::new(error.to_string()))?;
    Ok(CloudServiceAccount {
        snapshot: CloudAccountSnapshot {
            account_key,
            generation: CloudAccountGeneration::new(snapshot.account_generation.get()),
            connected: cloud.connected(),
            host_url: cloud.host_url.clone(),
            public_url: cloud.public_url.clone(),
            username: cloud.connected_username.clone(),
            display_name: cloud.connected_display_name.clone(),
            user_id: cloud.connected_user_id.clone(),
            default_visibility: cloud.default_visibility.clone(),
            delete_local_after_upload: cloud.delete_local_after_upload,
            auto_upload_rules: cloud.auto_upload_rules,
        },
        credential_target: cloud.credential_target.clone(),
        local_paths_by_clip_id,
    })
}

/// [`SettingsStore`]-backed account/profile adapter shared by native shells.
#[derive(Clone)]
pub struct SettingsCloudAccountPort {
    store: SettingsStore,
}

impl SettingsCloudAccountPort {
    #[must_use]
    pub fn new(store: SettingsStore) -> Self {
        Self { store }
    }

    fn settings_snapshot(&self) -> Result<SettingsSnapshot, PortError> {
        run_settings_io(|| self.store.snapshot().map_err(settings_error))
    }
}

impl CloudAccountPort for SettingsCloudAccountPort {
    fn snapshot(&self) -> Result<CloudServiceAccount, PortError> {
        cloud_service_account_from_snapshot(&self.settings_snapshot()?)
    }

    fn apply_profile(
        &self,
        expected_key: &CloudAccountKey,
        expected_generation: CloudAccountGeneration,
        patch: CloudProfilePatch,
    ) -> Result<CloudServiceAccount, PortError> {
        let before = self.settings_snapshot()?;
        let current = cloud_service_account_from_snapshot(&before)?;
        if &current.snapshot.account_key != expected_key
            || current.snapshot.generation != expected_generation
            || current.snapshot.user_id.as_deref() != Some(patch.user_id.as_str())
        {
            return Err(PortError::account_changed());
        }
        validate_profile_patch(&patch)?;
        let change = CloudProfileCas {
            account: before.account,
            account_generation: before.account_generation,
            expected_connected_user_id: patch.user_id,
            username: patch.username,
            display_name: patch.display_name,
        };
        let after = run_settings_io(|| {
            self.store
                .compare_exchange_cloud_profile(change)
                .map_err(settings_error)
        })?;
        cloud_service_account_from_snapshot(&after)
    }
}

fn validate_catalog_text(field: &str, value: &str) -> Result<(), PortError> {
    if value.trim().is_empty() {
        return Err(PortError::new(format!("{field} is missing")));
    }
    if value.len() > MAX_CATALOG_STRING_BYTES {
        return Err(PortError::new(format!(
            "{field} is {} bytes; maximum is {MAX_CATALOG_STRING_BYTES}",
            value.len()
        )));
    }
    Ok(())
}

fn validate_profile_patch(patch: &CloudProfilePatch) -> Result<(), PortError> {
    validate_catalog_text("cloud profile user id", &patch.user_id)?;
    validate_catalog_text("cloud profile username", &patch.username)?;
    if let Some(display_name) = patch.display_name.as_deref() {
        validate_catalog_text("cloud profile display name", display_name)?;
    }
    Ok(())
}

fn validate_optional_catalog_text(field: &str, value: Option<&str>) -> Result<(), PortError> {
    value.map_or(Ok(()), |value| validate_catalog_text(field, value))
}

fn settings_error(error: SettingsTransactionError) -> PortError {
    match error {
        SettingsTransactionError::AccountChanged
        | SettingsTransactionError::StaleAccountGeneration { .. }
        | SettingsTransactionError::AccountGenerationExhausted => PortError::account_changed(),
        other => PortError::new(other.to_string()),
    }
}

fn run_settings_io<T>(operation: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current().map(|handle| handle.runtime_flavor()) {
        Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(operation),
        _ => operation(),
    }
}
