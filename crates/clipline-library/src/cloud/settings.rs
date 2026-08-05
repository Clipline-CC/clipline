//! Whole-settings adapters for the framework-neutral Cloud account service.

use std::collections::btree_map::Entry;
use std::collections::BTreeMap;

use clipline_settings::{
    CloudAccountCas, CloudAccountCasKind, CloudAccountProfile, CloudAccountPublicationOwner,
    CloudProfileCas, SettingsSnapshot, SettingsStore, SettingsTransactionError,
    MAX_CLOUD_CREDENTIAL_CLEANUP_TARGETS,
};

use crate::{
    account_key, CloudAccountFields, CloudAccountGeneration, CloudAccountKey, CloudAccountSnapshot,
    MAX_CATALOG_STRING_BYTES, MAX_CLOUD_INDEX_ROWS,
};

use super::account::{CloudAccountMutationPort, CloudAccountState, CloudConnectedProfile};
use super::ports::{CloudAccountPort, CloudProfilePatch, PortError};
use super::CloudServiceAccount;
use super::{
    cache::{AccountPublicationGuard, CloudCacheError},
    cache_identity::{CloudAccountFence, CloudCacheNamespace},
};

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

/// Derive the exact cache publication fence from one bounded durable snapshot.
pub fn cloud_cache_account_fence_from_snapshot(
    snapshot: &SettingsSnapshot,
) -> Result<CloudAccountFence, PortError> {
    let account = cloud_service_account_from_snapshot(snapshot)?;
    cloud_cache_account_fence_from_service_account(&account)
}

/// Derive the shipping-compatible cache namespace from a bounded neutral
/// service account.
///
/// Stable namespace identity prefers user id, then the legacy username, then
/// credential target. The account key is re-derived and compared so a caller
/// cannot combine a forged key with otherwise valid account fields.
pub fn cloud_cache_account_fence_from_service_account(
    account: &CloudServiceAccount,
) -> Result<CloudAccountFence, PortError> {
    let snapshot = &account.snapshot;
    validate_catalog_text("cloud host URL", &snapshot.host_url)?;
    validate_optional_catalog_text("cloud username", snapshot.username.as_deref())?;
    validate_optional_catalog_text("cloud user id", snapshot.user_id.as_deref())?;
    validate_optional_catalog_text(
        "cloud credential target",
        account.credential_target.as_deref(),
    )?;
    let derived_key = account_key(&CloudAccountFields {
        host_url: snapshot.host_url.clone(),
        connected_user_id: snapshot.user_id.clone().unwrap_or_default(),
        credential_target: account.credential_target.clone().unwrap_or_default(),
    })
    .map_err(|error| PortError::new(error.to_string()))?;
    if derived_key != snapshot.account_key {
        return Err(PortError::new(
            "cloud service account key does not match its durable fields",
        ));
    }
    let stable_account = snapshot
        .user_id
        .as_deref()
        .or(snapshot.username.as_deref())
        .or(account.credential_target.as_deref())
        .ok_or_else(|| PortError::new("cloud cache account identity is unavailable"))?;
    let cache_namespace = CloudCacheNamespace::derive(&snapshot.host_url, stable_account)
        .map_err(|error| PortError::new(error.to_string()))?;
    Ok(CloudAccountFence {
        account_key: derived_key,
        account_generation: snapshot.generation,
        cache_namespace,
    })
}

/// Settings-backed final-publication gate shared by native shells.
#[derive(Clone)]
pub struct SettingsAccountPublicationGuard {
    store: SettingsStore,
}

impl SettingsAccountPublicationGuard {
    #[must_use]
    pub fn new(store: SettingsStore) -> Self {
        Self { store }
    }

    fn expected_owner(
        &self,
        requested: &CloudAccountFence,
    ) -> Result<CloudAccountPublicationOwner, CloudCacheError> {
        let snapshot = run_settings_io(|| self.store.snapshot().map_err(cache_settings_error))?;
        let current = cloud_cache_account_fence_from_snapshot(&snapshot)
            .map_err(|error| CloudCacheError::Internal(error.to_string()))?;
        if &current != requested {
            return Err(CloudCacheError::StaleAccount);
        }
        Ok(CloudAccountPublicationOwner::from_snapshot(&snapshot))
    }
}

impl AccountPublicationGuard for SettingsAccountPublicationGuard {
    fn is_current(&self, account: &CloudAccountFence) -> bool {
        let Ok(owner) = self.expected_owner(account) else {
            return false;
        };
        run_settings_io(|| {
            matches!(
                self.store
                    .publish_if_cloud_account_current(&owner, || Ok::<(), ()>(())),
                Ok(Ok(()))
            )
        })
    }

    fn publish_if_current(
        &self,
        account: &CloudAccountFence,
        publication: &mut dyn FnMut() -> Result<(), CloudCacheError>,
    ) -> Result<(), CloudCacheError> {
        let owner = self.expected_owner(account)?;
        match run_settings_io(|| {
            self.store
                .publish_if_cloud_account_current(&owner, publication)
        }) {
            Ok(result) => result,
            Err(error) => Err(cache_settings_error(error)),
        }
    }
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

    fn current_account_snapshot(&self) -> Result<SettingsSnapshot, PortError> {
        run_settings_io(|| self.store.current_cloud_account().map_err(settings_error))
    }

    fn state_from_snapshot(snapshot: &SettingsSnapshot) -> Result<CloudAccountState, PortError> {
        Ok(CloudAccountState {
            account: cloud_service_account_from_snapshot(snapshot)?,
            profile: CloudAccountProfile::from_settings(&snapshot.document.cloud),
        })
    }

    fn exact_before(&self, expected: &CloudAccountState) -> Result<SettingsSnapshot, PortError> {
        let before = self.current_account_snapshot()?;
        let current = Self::state_from_snapshot(&before)?;
        if current.account.snapshot.account_key != expected.account.snapshot.account_key
            || current.account.snapshot.generation != expected.account.snapshot.generation
            || current.profile != expected.profile
        {
            return Err(PortError::account_changed());
        }
        Ok(before)
    }

    fn commit_account(
        &self,
        before: &SettingsSnapshot,
        kind: CloudAccountCasKind,
        expected: CloudAccountProfile,
        replacement: CloudAccountProfile,
        default_visibility: Option<String>,
    ) -> Result<CloudAccountState, PortError> {
        let change = CloudAccountCas {
            kind,
            expected_account: before.account.clone(),
            expected_account_generation: before.account_generation,
            expected,
            replacement,
            default_visibility,
        };
        let after = run_settings_io(|| {
            self.store
                .compare_exchange_cloud_account(change)
                .map_err(settings_error)
        })?;
        Self::state_from_snapshot(&after)
    }
}

impl CloudAccountMutationPort for SettingsCloudAccountPort {
    fn load(&self) -> Result<CloudAccountState, PortError> {
        let snapshot = self.current_account_snapshot()?;
        Self::state_from_snapshot(&snapshot)
    }

    fn reserve_credential(
        &self,
        expected: &CloudAccountState,
        target: String,
    ) -> Result<CloudAccountState, PortError> {
        let before = self.exact_before(expected)?;
        let mut replacement = expected.profile.clone();
        if replacement.credential_cleanup_targets.contains(&target)
            || replacement.credential_target.as_ref() == Some(&target)
        {
            return Err(PortError::new("cloud credential target collision"));
        }
        if replacement.credential_cleanup_targets.len() >= MAX_CLOUD_CREDENTIAL_CLEANUP_TARGETS {
            return Err(PortError::new("cloud credential cleanup queue is full"));
        }
        replacement.credential_cleanup_targets.push(target);
        replacement.normalize();
        self.commit_account(
            &before,
            CloudAccountCasKind::ReserveCredential,
            expected.profile.clone(),
            replacement,
            None,
        )
    }

    fn commit_connect(
        &self,
        expected: &CloudAccountState,
        connected: CloudConnectedProfile,
        default_visibility: Option<String>,
    ) -> Result<CloudAccountState, PortError> {
        let before = self.exact_before(expected)?;
        if !expected
            .profile
            .credential_cleanup_targets
            .contains(&connected.credential_target)
        {
            return Err(PortError::new(
                "cloud candidate credential was not durably reserved",
            ));
        }
        let mut replacement = expected.profile.clone();
        replacement.host_url = connected.host_url;
        replacement.public_url = Some(connected.public_url);
        replacement.connected_user_id = Some(connected.user_id);
        replacement.connected_username = Some(connected.username);
        replacement.connected_display_name = connected.display_name;
        replacement.credential_target = Some(connected.credential_target.clone());
        replacement
            .credential_cleanup_targets
            .retain(|target| target != &connected.credential_target);
        if let Some(previous) = expected
            .profile
            .credential_target
            .as_ref()
            .filter(|previous| *previous != &connected.credential_target)
        {
            if !replacement.credential_cleanup_targets.contains(previous) {
                if replacement.credential_cleanup_targets.len()
                    >= MAX_CLOUD_CREDENTIAL_CLEANUP_TARGETS
                {
                    return Err(PortError::new("cloud credential cleanup queue is full"));
                }
                replacement
                    .credential_cleanup_targets
                    .push(previous.clone());
            }
        }
        replacement.normalize();
        self.commit_account(
            &before,
            CloudAccountCasKind::Connect,
            expected.profile.clone(),
            replacement,
            default_visibility,
        )
    }

    fn commit_disconnect(
        &self,
        expected: &CloudAccountState,
    ) -> Result<CloudAccountState, PortError> {
        let before = self.exact_before(expected)?;
        let mut replacement = expected.profile.clone();
        if let Some(previous) = replacement.credential_target.take() {
            if !replacement.credential_cleanup_targets.contains(&previous) {
                if replacement.credential_cleanup_targets.len()
                    >= MAX_CLOUD_CREDENTIAL_CLEANUP_TARGETS
                {
                    return Err(PortError::new("cloud credential cleanup queue is full"));
                }
                replacement.credential_cleanup_targets.push(previous);
            }
        }
        replacement.connected_user_id = None;
        replacement.connected_username = None;
        replacement.connected_display_name = None;
        replacement.normalize();
        self.commit_account(
            &before,
            CloudAccountCasKind::Disconnect,
            expected.profile.clone(),
            replacement,
            None,
        )
    }

    fn reconcile_cleanup(
        &self,
        expected: &CloudAccountState,
        deleted_targets: &[String],
    ) -> Result<CloudAccountState, PortError> {
        let before = self.exact_before(expected)?;
        let mut replacement = expected.profile.clone();
        replacement
            .credential_cleanup_targets
            .retain(|target| !deleted_targets.contains(target));
        replacement.normalize();
        self.commit_account(
            &before,
            CloudAccountCasKind::ReconcileCleanup,
            expected.profile.clone(),
            replacement,
            None,
        )
    }
}

impl CloudAccountPort for SettingsCloudAccountPort {
    fn snapshot(&self) -> Result<CloudServiceAccount, PortError> {
        cloud_service_account_from_snapshot(&self.current_account_snapshot()?)
    }

    fn apply_profile(
        &self,
        expected_key: &CloudAccountKey,
        expected_generation: CloudAccountGeneration,
        patch: CloudProfilePatch,
    ) -> Result<CloudServiceAccount, PortError> {
        let before = self.current_account_snapshot()?;
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
        | SettingsTransactionError::StaleCloudProfile
        | SettingsTransactionError::StaleAccountGeneration { .. }
        | SettingsTransactionError::AccountGenerationExhausted => PortError::account_changed(),
        other => PortError::new(other.to_string()),
    }
}

fn cache_settings_error(error: SettingsTransactionError) -> CloudCacheError {
    match error {
        SettingsTransactionError::AccountChanged
        | SettingsTransactionError::StaleAccountGeneration { .. }
        | SettingsTransactionError::AccountGenerationExhausted => CloudCacheError::StaleAccount,
        other => CloudCacheError::Internal(other.to_string()),
    }
}

fn run_settings_io<T>(operation: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current().map(|handle| handle.runtime_flavor()) {
        Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(operation),
        _ => operation(),
    }
}
