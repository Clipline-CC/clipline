//! Reusable whole-settings adapters for durable upload ownership.

use std::collections::HashMap;
use std::sync::Mutex;

use clipline_settings::{
    cloud_paths_equivalent, CloudAccountIdentity, CloudRecordCas, CloudRecordCasKind,
    CloudRecordSlot, CloudSettings, CloudUploadRecord, SettingsSnapshot, SettingsStore,
    SettingsTransactionError, MAX_CLOUD_RECORD_CAS_SLOTS,
};

use crate::{
    account_key, ClipPathIdentity, CloudAccountFields, CloudAccountGeneration, DurableUploadToken,
    LocalClipId, UploadAccountFence, UploadAccountOwner, UploadGeneration, UploadPhase,
    UploadRecord, UploadRecordCursor, UploadRecordError, UploadRecordErrorKind, UploadRecordPort,
    ValidatedClipPath, MAX_ACTIVE_UPLOAD_JOBS,
};

/// Derive the exact durable upload account owner from one settings snapshot.
pub fn upload_account_owner_from_snapshot(
    snapshot: &SettingsSnapshot,
) -> Result<UploadAccountOwner, UploadRecordError> {
    if !snapshot.document.cloud.connected() {
        return Err(record_error(
            UploadRecordErrorKind::AccountChanged,
            "Clipline Cloud is not connected",
        ));
    }
    let key = account_key(&CloudAccountFields {
        host_url: snapshot.document.cloud.host_url.clone(),
        connected_user_id: snapshot
            .document
            .cloud
            .connected_user_id
            .clone()
            .unwrap_or_default(),
        credential_target: snapshot
            .document
            .cloud
            .credential_target
            .clone()
            .unwrap_or_default(),
    })
    .map_err(|error| record_error(UploadRecordErrorKind::Persistence, error.to_string()))?;
    Ok(UploadAccountOwner::new(
        key,
        CloudAccountGeneration::new(snapshot.account_generation.get()),
    ))
}

/// Account fence backed directly by the process-owned whole-settings store.
#[derive(Clone)]
pub struct SettingsUploadAccountFence {
    store: SettingsStore,
}

impl SettingsUploadAccountFence {
    #[must_use]
    pub fn new(store: SettingsStore) -> Self {
        Self { store }
    }
}

impl UploadAccountFence for SettingsUploadAccountFence {
    fn is_current(&self, owner: &UploadAccountOwner) -> bool {
        self.store
            .snapshot()
            .ok()
            .and_then(|snapshot| upload_account_owner_from_snapshot(&snapshot).ok())
            .as_ref()
            == Some(owner)
    }
}

/// Settings-backed exact-CAS record port used by every native shell adapter.
///
/// The durable compatibility schema omits volatile byte counters and phase
/// detail. A bounded live overlay retains those fields only while its exact
/// durable projection remains current.
pub struct SettingsUploadRecordPort {
    store: SettingsStore,
    live: Mutex<HashMap<DurableUploadToken, UploadRecord>>,
}

impl SettingsUploadRecordPort {
    #[must_use]
    pub fn new(store: SettingsStore) -> Self {
        Self {
            store,
            live: Mutex::new(HashMap::with_capacity(MAX_ACTIVE_UPLOAD_JOBS)),
        }
    }

    fn snapshot(&self) -> Result<SettingsSnapshot, UploadRecordError> {
        run_settings_io(|| self.store.snapshot().map_err(classify_settings_error))
    }

    fn commit(&self, change: CloudRecordCas) -> Result<SettingsSnapshot, UploadRecordError> {
        run_settings_io(|| {
            self.store
                .compare_exchange_cloud_records(change)
                .map_err(classify_settings_error)
        })
    }

    fn remember(&self, record: UploadRecord) -> Result<(), UploadRecordError> {
        let mut live = self.live.lock().map_err(|_| {
            record_error(
                UploadRecordErrorKind::Persistence,
                "live upload record cache is unavailable",
            )
        })?;
        remember_live_record(&mut live, record);
        Ok(())
    }

    fn load_live_if_exact(
        &self,
        durable: &CloudUploadRecord,
        reloaded: &UploadRecord,
    ) -> Result<UploadRecord, UploadRecordError> {
        let mut live = self.live.lock().map_err(|_| {
            record_error(
                UploadRecordErrorKind::Persistence,
                "live upload record cache is unavailable",
            )
        })?;
        Ok(load_live_record(&mut live, durable, reloaded))
    }
}

impl UploadRecordPort for SettingsUploadRecordPort {
    fn allocate_generation(
        &self,
        owner: &UploadAccountOwner,
        local_clip_id: &LocalClipId,
        source: &ValidatedClipPath,
    ) -> Result<UploadGeneration, UploadRecordError> {
        let snapshot = self.snapshot()?;
        ensure_snapshot_owner(&snapshot, owner)?;
        next_generation(
            &snapshot.document.cloud,
            local_clip_id,
            source.display_path(),
            source.comparison_identity(),
        )
    }

    fn load(
        &self,
        owner: &UploadAccountOwner,
        local_clip_id: &LocalClipId,
    ) -> Result<Option<UploadRecordCursor>, UploadRecordError> {
        let snapshot = self.snapshot()?;
        ensure_snapshot_owner(&snapshot, owner)?;
        let Some(durable) = snapshot.document.cloud.uploads.get(local_clip_id.as_str()) else {
            return Ok(None);
        };
        let reloaded = reload_upload_record(owner, durable)?;
        let record = self.load_live_if_exact(durable, &reloaded)?;
        Ok(Some(UploadRecordCursor {
            revision: snapshot.revision.get(),
            record,
        }))
    }

    fn admit(&self, record: UploadRecord) -> Result<UploadRecordCursor, UploadRecordError> {
        let snapshot = self.snapshot()?;
        let owner = owner_from_record(&record);
        ensure_snapshot_owner(&snapshot, &owner)?;
        let change = admission_cas(&snapshot, &record)?;
        let after = self.commit(change)?;
        ensure_snapshot_owner(&after, &owner)?;
        self.remember(record.clone())?;
        Ok(UploadRecordCursor {
            revision: after.revision.get(),
            record,
        })
    }

    fn compare_exchange(
        &self,
        expected: &UploadRecordCursor,
        replacement: UploadRecord,
    ) -> Result<UploadRecordCursor, UploadRecordError> {
        if expected.record.token != replacement.token {
            return Err(record_error(
                UploadRecordErrorKind::Superseded,
                "upload record replacement changed its durable owner",
            ));
        }
        let snapshot = self.snapshot()?;
        let owner = owner_from_record(&expected.record);
        ensure_snapshot_owner(&snapshot, &owner)?;
        let change = transition_cas(&snapshot, &expected.record, &replacement)?;
        let after = self.commit(change)?;
        ensure_snapshot_owner(&after, &owner)?;
        self.remember(replacement.clone())?;
        Ok(UploadRecordCursor {
            revision: after.revision.get(),
            record: replacement,
        })
    }

    fn remove_exact(&self, expected: &UploadRecordCursor) -> Result<(), UploadRecordError> {
        let snapshot = self.snapshot()?;
        let owner = owner_from_record(&expected.record);
        ensure_snapshot_owner(&snapshot, &owner)?;
        let change = removal_cas(&snapshot, &expected.record)?;
        let after = self.commit(change)?;
        ensure_snapshot_owner(&after, &owner)?;
        // Durability is authoritative. A poisoned optimization cache cannot
        // turn a committed removal into a reported failure.
        if let Ok(mut live) = self.live.lock() {
            live.remove(&expected.record.token);
        }
        Ok(())
    }
}

fn run_settings_io<T>(operation: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current().map(|handle| handle.runtime_flavor()) {
        Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(operation),
        _ => operation(),
    }
}

fn remember_live_record(
    live: &mut HashMap<DurableUploadToken, UploadRecord>,
    record: UploadRecord,
) {
    let token = record.token.clone();
    live.retain(|existing, _| {
        existing.account_key != token.account_key
            || existing.account_generation != token.account_generation
            || existing.local_clip_id != token.local_clip_id
    });
    if live.len() >= MAX_ACTIVE_UPLOAD_JOBS {
        let evicted_terminal = live.iter().find_map(|(existing, cached)| {
            (existing != &token && cached.phase.is_terminal()).then(|| existing.clone())
        });
        if let Some(evicted) = evicted_terminal {
            live.remove(&evicted);
        }
    }
    if live.len() >= MAX_ACTIVE_UPLOAD_JOBS {
        if let Some(evicted) = live.keys().find(|existing| *existing != &token).cloned() {
            live.remove(&evicted);
        }
    }
    live.insert(token, record);
}

fn load_live_record(
    live: &mut HashMap<DurableUploadToken, UploadRecord>,
    durable: &CloudUploadRecord,
    reloaded: &UploadRecord,
) -> UploadRecord {
    match live.get(&reloaded.token) {
        Some(cached) if upload_record_to_cloud_record(cached) == *durable => cached.clone(),
        Some(_) => {
            live.remove(&reloaded.token);
            reloaded.clone()
        }
        None => reloaded.clone(),
    }
}

fn ensure_snapshot_owner(
    snapshot: &SettingsSnapshot,
    expected: &UploadAccountOwner,
) -> Result<(), UploadRecordError> {
    if upload_account_owner_from_snapshot(snapshot)? == *expected {
        Ok(())
    } else {
        Err(record_error(
            UploadRecordErrorKind::AccountChanged,
            "cloud account changed while upload state was in flight",
        ))
    }
}

fn owner_from_record(record: &UploadRecord) -> UploadAccountOwner {
    UploadAccountOwner::new(
        record.token.account_key.clone(),
        record.token.account_generation,
    )
}

fn next_generation(
    cloud: &CloudSettings,
    local_clip_id: &LocalClipId,
    source_display_path: &str,
    source_identity: &ClipPathIdentity,
) -> Result<UploadGeneration, UploadRecordError> {
    let maximum = cloud
        .uploads
        .iter()
        .filter(|(key, record)| {
            key.as_str() == local_clip_id.as_str()
                || record.local_clip_id == local_clip_id.as_str()
                || cloud_paths_equivalent(&record.path, source_display_path)
                || ClipPathIdentity::from_text(&record.path).as_ref() == Some(source_identity)
        })
        .filter_map(|(_, record)| record.upload_generation)
        .max()
        .unwrap_or(0)
        .max(cloud.upload_generation_sequence);
    maximum
        .checked_add(1)
        .map(UploadGeneration::new)
        .ok_or_else(|| {
            record_error(
                UploadRecordErrorKind::Persistence,
                "durable upload generation is exhausted",
            )
        })
}

fn admission_cas(
    snapshot: &SettingsSnapshot,
    record: &UploadRecord,
) -> Result<CloudRecordCas, UploadRecordError> {
    let replacement = upload_record_to_cloud_record(record);
    let stable_key = record.token.local_clip_id.as_str();
    let expected = reconciled_slots(
        &snapshot.document.cloud,
        stable_key,
        &record.path,
        None,
        true,
    )?;
    let durable_maximum = expected
        .iter()
        .filter_map(|slot| slot.record.as_ref()?.upload_generation)
        .max()
        .unwrap_or(0)
        .max(snapshot.document.cloud.upload_generation_sequence);
    if record.token.upload_generation.get() <= durable_maximum {
        return Err(record_error(
            UploadRecordErrorKind::Contended,
            "a newer durable upload generation appeared before admission",
        ));
    }
    Ok(CloudRecordCas {
        account: CloudAccountIdentity::from_settings(&snapshot.document.cloud),
        account_generation: snapshot.account_generation,
        kind: CloudRecordCasKind::Admit {
            upload_generation: record.token.upload_generation.get(),
        },
        expected,
        replacement: Some(CloudRecordSlot {
            key: stable_key.to_owned(),
            record: Some(replacement),
        }),
    })
}

fn transition_cas(
    snapshot: &SettingsSnapshot,
    expected_record: &UploadRecord,
    replacement: &UploadRecord,
) -> Result<CloudRecordCas, UploadRecordError> {
    let stable_key = expected_record.token.local_clip_id.as_str();
    let expected_durable = upload_record_to_cloud_record(expected_record);
    if snapshot.document.cloud.uploads.get(stable_key) != Some(&expected_durable) {
        return Err(record_error(
            UploadRecordErrorKind::Superseded,
            "durable upload record changed before compare-and-swap",
        ));
    }
    let expected = reconciled_slots(
        &snapshot.document.cloud,
        stable_key,
        &replacement.path,
        Some(&expected_record.path),
        false,
    )?;
    let kind = if expected_record.token.upload_generation == UploadGeneration::INITIAL
        || expected_record.phase.is_terminal()
    {
        CloudRecordCasKind::StatusSync
    } else {
        CloudRecordCasKind::Advance {
            upload_generation: replacement.token.upload_generation.get(),
        }
    };
    Ok(CloudRecordCas {
        account: CloudAccountIdentity::from_settings(&snapshot.document.cloud),
        account_generation: snapshot.account_generation,
        kind,
        expected,
        replacement: Some(CloudRecordSlot {
            key: stable_key.to_owned(),
            record: Some(upload_record_to_cloud_record(replacement)),
        }),
    })
}

fn removal_cas(
    snapshot: &SettingsSnapshot,
    expected_record: &UploadRecord,
) -> Result<CloudRecordCas, UploadRecordError> {
    let stable_key = expected_record.token.local_clip_id.as_str();
    let expected_durable = upload_record_to_cloud_record(expected_record);
    if snapshot.document.cloud.uploads.get(stable_key) != Some(&expected_durable) {
        return Err(record_error(
            UploadRecordErrorKind::Superseded,
            "durable upload record is missing",
        ));
    }
    let expected = reconciled_slots(
        &snapshot.document.cloud,
        stable_key,
        &expected_record.path,
        Some(&expected_record.path),
        false,
    )?;
    Ok(CloudRecordCas {
        account: CloudAccountIdentity::from_settings(&snapshot.document.cloud),
        account_generation: snapshot.account_generation,
        kind: CloudRecordCasKind::StatusSync,
        expected,
        replacement: None,
    })
}

fn reconciled_slots(
    cloud: &CloudSettings,
    stable_key: &str,
    replacement_path: &str,
    prior_path: Option<&str>,
    include_absent_stable: bool,
) -> Result<Vec<CloudRecordSlot>, UploadRecordError> {
    let mut expected = Vec::new();
    for (key, record) in &cloud.uploads {
        let path_alias = cloud_paths_equivalent(&record.path, replacement_path)
            || prior_path.is_some_and(|prior| cloud_paths_equivalent(&record.path, prior));
        if key == stable_key || path_alias {
            expected.push(CloudRecordSlot {
                key: key.clone(),
                record: Some(record.clone()),
            });
        }
    }
    if !expected.iter().any(|slot| slot.key == stable_key) {
        if include_absent_stable {
            expected.push(CloudRecordSlot {
                key: stable_key.to_owned(),
                record: None,
            });
        } else {
            return Err(record_error(
                UploadRecordErrorKind::Superseded,
                "durable upload record is missing",
            ));
        }
    }
    if expected.len() > MAX_CLOUD_RECORD_CAS_SLOTS {
        return Err(record_error(
            UploadRecordErrorKind::Persistence,
            format!(
                "upload path has {} durable aliases; maximum is {MAX_CLOUD_RECORD_CAS_SLOTS}",
                expected.len()
            ),
        ));
    }
    expected.sort_by(|left, right| left.key.cmp(&right.key));
    Ok(expected)
}

/// Project a full live record into the shipping compatibility schema.
#[must_use]
pub fn upload_record_to_cloud_record(record: &UploadRecord) -> CloudUploadRecord {
    let mut durable = CloudUploadRecord {
        local_clip_id: record.token.local_clip_id.as_str().to_owned(),
        client_clip_id: record
            .client_clip_id
            .as_ref()
            .map(|id| id.as_str().to_owned()),
        upload_generation: (record.token.upload_generation != UploadGeneration::INITIAL)
            .then(|| record.token.upload_generation.get()),
        path: record.path.clone(),
        remote_clip_id: record.remote_clip_id.clone(),
        remote_url: record.remote_url.clone(),
        visibility: record.visibility.clone(),
        upload_status: record.upload_status.clone(),
        error: record.error.clone(),
        updated_at_unix: record.updated_at_unix,
    };
    durable.normalize();
    durable
}

/// Reload a compatibility record without inventing volatile progress.
pub fn reload_upload_record(
    owner: &UploadAccountOwner,
    durable: &CloudUploadRecord,
) -> Result<UploadRecord, UploadRecordError> {
    let local_clip_id = LocalClipId::new(durable.local_clip_id.clone())
        .map_err(|error| record_error(UploadRecordErrorKind::Persistence, error.to_string()))?;
    let client_clip_id = durable
        .client_clip_id
        .clone()
        .map(crate::ClientClipId::new)
        .transpose()
        .map_err(|error| record_error(UploadRecordErrorKind::Persistence, error.to_string()))?;
    let source_path = ClipPathIdentity::from_text(&durable.path).ok_or_else(|| {
        record_error(
            UploadRecordErrorKind::Persistence,
            "durable upload record has an invalid source path",
        )
    })?;
    let phase = phase_for_persisted_status(&durable.upload_status)?;
    Ok(UploadRecord {
        token: DurableUploadToken {
            account_key: owner.account_key.clone(),
            account_generation: owner.account_generation,
            upload_generation: UploadGeneration::new(durable.upload_generation.unwrap_or(0)),
            local_clip_id,
            source_path,
        },
        client_clip_id,
        path: durable.path.clone(),
        visibility: durable.visibility.clone(),
        phase,
        upload_status: durable.upload_status.clone(),
        received_size_bytes: 0,
        file_size_bytes: 0,
        remote_clip_id: durable.remote_clip_id.clone(),
        remote_url: durable.remote_url.clone(),
        error: durable.error.clone(),
        local_deleted: false,
        updated_at_unix: durable.updated_at_unix,
    })
}

fn phase_for_persisted_status(status: &str) -> Result<UploadPhase, UploadRecordError> {
    match status {
        "queued" | "retrying" => Ok(UploadPhase::Queued),
        "uploading" => Ok(UploadPhase::Uploading),
        "processing" => Ok(UploadPhase::Processing),
        "uploaded_processing" => Ok(UploadPhase::Abandoned),
        "uploaded_private" | "uploaded_public" => Ok(UploadPhase::Completed),
        "failed" => Ok(UploadPhase::Failed),
        "canceled" => Ok(UploadPhase::Canceled),
        other => Err(record_error(
            UploadRecordErrorKind::Persistence,
            format!("durable upload status {other:?} cannot be reloaded"),
        )),
    }
}

fn classify_settings_error(error: SettingsTransactionError) -> UploadRecordError {
    let message = error.to_string();
    let kind = match &error {
        SettingsTransactionError::AccountChanged
        | SettingsTransactionError::StaleAccountGeneration { .. }
        | SettingsTransactionError::AccountGenerationExhausted => {
            UploadRecordErrorKind::AccountChanged
        }
        SettingsTransactionError::StaleCloudRecord => UploadRecordErrorKind::Superseded,
        SettingsTransactionError::StaleRevision { .. }
        | SettingsTransactionError::StalePreferences => UploadRecordErrorKind::Contended,
        SettingsTransactionError::RevisionExhausted
        | SettingsTransactionError::OsuAccountGenerationExhausted
        | SettingsTransactionError::StaleOsuProfile
        | SettingsTransactionError::ExternalModification
        | SettingsTransactionError::LockPoisoned => UploadRecordErrorKind::Persistence,
        SettingsTransactionError::Validation(_) | SettingsTransactionError::Persistence(_) => {
            // Preserve the shipping adapter's classification for nested
            // transaction messages while using typed variants where possible.
            let lower = message.to_ascii_lowercase();
            if lower.contains("account changed") || lower.contains("cloud account generation") {
                UploadRecordErrorKind::AccountChanged
            } else if lower.contains("stale cloud record") {
                UploadRecordErrorKind::Superseded
            } else if lower.contains("stale settings revision") || lower.contains("contended") {
                UploadRecordErrorKind::Contended
            } else {
                UploadRecordErrorKind::Persistence
            }
        }
    };
    record_error(kind, message)
}

fn record_error(kind: UploadRecordErrorKind, message: impl Into<String>) -> UploadRecordError {
    UploadRecordError::new(kind, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClientClipId;
    use clipline_settings::{AccountGeneration, AppSettings, SettingsRevision};

    fn connected_snapshot() -> SettingsSnapshot {
        let mut document = AppSettings::default();
        document.cloud.host_url = "https://cloud.example".into();
        document.cloud.connected_user_id = Some("user-1".into());
        document.cloud.credential_target = Some("credential-1".into());
        SettingsSnapshot {
            account: CloudAccountIdentity::from_settings(&document.cloud),
            document,
            revision: SettingsRevision::INITIAL,
            account_generation: AccountGeneration::INITIAL,
        }
    }

    fn owner(snapshot: &SettingsSnapshot) -> UploadAccountOwner {
        upload_account_owner_from_snapshot(snapshot).unwrap()
    }

    fn record(snapshot: &SettingsSnapshot, generation: u64, path: &str) -> UploadRecord {
        let owner = owner(snapshot);
        UploadRecord {
            token: DurableUploadToken {
                account_key: owner.account_key,
                account_generation: owner.account_generation,
                upload_generation: UploadGeneration::new(generation),
                local_clip_id: LocalClipId::new("local-1").unwrap(),
                source_path: ClipPathIdentity::from_text(path).unwrap(),
            },
            client_clip_id: None,
            path: path.into(),
            visibility: "private".into(),
            phase: UploadPhase::Queued,
            upload_status: "queued".into(),
            received_size_bytes: 0,
            file_size_bytes: 0,
            remote_clip_id: None,
            remote_url: None,
            error: None,
            local_deleted: false,
            updated_at_unix: 10,
        }
    }

    #[test]
    fn legacy_status_reload_mapping_is_explicit_and_fail_closed() {
        for (status, phase) in [
            ("queued", UploadPhase::Queued),
            ("retrying", UploadPhase::Queued),
            ("uploading", UploadPhase::Uploading),
            ("processing", UploadPhase::Processing),
            ("uploaded_processing", UploadPhase::Abandoned),
            ("uploaded_private", UploadPhase::Completed),
            ("uploaded_public", UploadPhase::Completed),
            ("failed", UploadPhase::Failed),
            ("canceled", UploadPhase::Canceled),
        ] {
            assert_eq!(phase_for_persisted_status(status).unwrap(), phase);
        }
        assert_eq!(
            phase_for_persisted_status("not_uploaded")
                .unwrap_err()
                .kind(),
            UploadRecordErrorKind::Persistence
        );
    }

    #[test]
    fn admission_reconciles_windows_aliases_in_one_exact_cas() {
        let mut snapshot = connected_snapshot();
        let mut legacy =
            upload_record_to_cloud_record(&record(&snapshot, 7, r"\\?\C:\Media\Clip.mp4"));
        legacy.local_clip_id = "legacy-client-id".into();
        snapshot
            .document
            .cloud
            .uploads
            .insert("legacy-client-id".into(), legacy.clone());
        snapshot.document.cloud.uploads.insert(
            "unrelated".into(),
            CloudUploadRecord {
                local_clip_id: "unrelated".into(),
                path: r"C:\Media\other.mp4".into(),
                ..legacy.clone()
            },
        );
        let admitted = record(&snapshot, 8, r"c:\media\CLIP.mp4");

        let cas = admission_cas(&snapshot, &admitted).unwrap();

        assert_eq!(
            cas.kind,
            CloudRecordCasKind::Admit {
                upload_generation: 8
            }
        );
        assert_eq!(cas.expected.len(), 2);
        assert!(cas.expected.iter().any(|slot| {
            slot.key == "legacy-client-id" && slot.record.as_ref() == Some(&legacy)
        }));
        assert!(cas
            .expected
            .iter()
            .any(|slot| slot.key == "local-1" && slot.record.is_none()));
        assert!(!cas.expected.iter().any(|slot| slot.key == "unrelated"));
        assert_eq!(cas.replacement.unwrap().key, "local-1");
    }

    #[test]
    fn generation_allocation_includes_path_equivalent_legacy_aliases() {
        let mut snapshot = connected_snapshot();
        let mut legacy =
            upload_record_to_cloud_record(&record(&snapshot, 41, r"C:\Media\Clip.mp4"));
        legacy.local_clip_id = "old-client-id".into();
        snapshot
            .document
            .cloud
            .uploads
            .insert("old-client-id".into(), legacy);

        let next = next_generation(
            &snapshot.document.cloud,
            &LocalClipId::new("local-1").unwrap(),
            r"\\?\c:\MEDIA\clip.mp4",
            &ClipPathIdentity::from_text(r"c:\media\clip.mp4").unwrap(),
        )
        .unwrap();

        assert_eq!(next, UploadGeneration::new(42));
    }

    #[test]
    fn admission_rechecks_generation_after_allocation_race() {
        let mut snapshot = connected_snapshot();
        snapshot.document.cloud.upload_generation_sequence = 42;
        let stale_candidate = record(&snapshot, 42, r"c:\media\clip.mp4");

        let error = admission_cas(&snapshot, &stale_candidate).unwrap_err();

        assert_eq!(error.kind(), UploadRecordErrorKind::Contended);
    }

    #[test]
    fn live_projection_round_trips_durable_fields_and_defaults_volatile_fields() {
        let snapshot = connected_snapshot();
        let mut live = record(&snapshot, 3, r"C:\Media\clip.mp4");
        live.client_clip_id = Some(ClientClipId::new("client-3").unwrap());
        live.phase = UploadPhase::Completed;
        live.upload_status = "uploaded_public".into();
        live.visibility = "public".into();
        live.received_size_bytes = 999;
        live.file_size_bytes = 999;
        live.remote_clip_id = Some("remote-3".into());
        live.remote_url = Some("https://cloud.example/clip/remote-3".into());
        live.local_deleted = true;
        let durable = upload_record_to_cloud_record(&live);

        let reloaded = reload_upload_record(&owner(&snapshot), &durable).unwrap();

        assert_eq!(upload_record_to_cloud_record(&reloaded), durable);
        assert_eq!(reloaded.phase, UploadPhase::Completed);
        assert_eq!(reloaded.received_size_bytes, 0);
        assert_eq!(reloaded.file_size_bytes, 0);
        assert!(!reloaded.local_deleted);
    }

    #[test]
    fn legacy_status_sync_preserves_absent_generation() {
        let mut snapshot = connected_snapshot();
        let mut legacy = upload_record_to_cloud_record(&record(&snapshot, 1, r"C:\Media\clip.mp4"));
        legacy.upload_generation = None;
        legacy.upload_status = "uploaded_processing".into();
        snapshot
            .document
            .cloud
            .uploads
            .insert("local-1".into(), legacy.clone());
        let expected = reload_upload_record(&owner(&snapshot), &legacy).unwrap();
        assert_eq!(expected.token.upload_generation, UploadGeneration::INITIAL);
        let mut replacement = expected.clone();
        replacement.phase = UploadPhase::Completed;
        replacement.upload_status = "uploaded_private".into();
        replacement.remote_clip_id = Some("remote-legacy".into());

        let cas = transition_cas(&snapshot, &expected, &replacement).unwrap();

        assert_eq!(cas.kind, CloudRecordCasKind::StatusSync);
        assert_eq!(
            cas.replacement.unwrap().record.unwrap().upload_generation,
            None
        );
    }

    #[test]
    fn exact_removal_includes_path_aliases_and_no_replacement() {
        let mut snapshot = connected_snapshot();
        let expected = record(&snapshot, 5, r"C:\Media\clip.mp4");
        let durable = upload_record_to_cloud_record(&expected);
        snapshot
            .document
            .cloud
            .uploads
            .insert("local-1".into(), durable.clone());
        let mut alias = durable.clone();
        alias.local_clip_id = "legacy-alias".into();
        alias.path = r"\\?\c:\MEDIA\CLIP.mp4".into();
        snapshot
            .document
            .cloud
            .uploads
            .insert("legacy-alias".into(), alias.clone());

        let cas = removal_cas(&snapshot, &expected).unwrap();

        assert_eq!(cas.kind, CloudRecordCasKind::StatusSync);
        assert!(cas.replacement.is_none());
        assert_eq!(cas.expected.len(), 2);
        assert!(cas
            .expected
            .iter()
            .any(|slot| slot.key == "local-1" && slot.record.as_ref() == Some(&durable)));
        assert!(cas
            .expected
            .iter()
            .any(|slot| slot.key == "legacy-alias" && slot.record.as_ref() == Some(&alias)));
    }

    #[test]
    fn exact_removal_preserves_legacy_none_generation_shape() {
        let mut snapshot = connected_snapshot();
        let mut durable =
            upload_record_to_cloud_record(&record(&snapshot, 1, r"C:\Media\legacy.mp4"));
        durable.upload_generation = None;
        durable.upload_status = "uploaded_processing".into();
        snapshot
            .document
            .cloud
            .uploads
            .insert("local-1".into(), durable.clone());
        let expected = reload_upload_record(&owner(&snapshot), &durable).unwrap();

        let cas = removal_cas(&snapshot, &expected).unwrap();

        assert_eq!(cas.expected.len(), 1);
        assert_eq!(cas.expected[0].record, Some(durable));
        assert!(cas.replacement.is_none());
    }

    #[test]
    fn exact_removal_rejects_stale_expected_record_shape() {
        let mut snapshot = connected_snapshot();
        let stale = record(&snapshot, 3, r"C:\Media\clip.mp4");
        let current = record(&snapshot, 4, r"C:\Media\clip.mp4");
        snapshot
            .document
            .cloud
            .uploads
            .insert("local-1".into(), upload_record_to_cloud_record(&current));

        let error = removal_cas(&snapshot, &stale).unwrap_err();

        assert_eq!(error.kind(), UploadRecordErrorKind::Superseded);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn settings_io_boundary_is_safe_inside_a_multithread_runtime() {
        assert_eq!(run_settings_io(|| 42), 42);
    }

    #[test]
    fn live_overlay_never_exceeds_the_active_upload_bound() {
        let owner = UploadAccountOwner::new(
            crate::CloudAccountKey::new("account").unwrap(),
            CloudAccountGeneration::new(1),
        );
        let mut live = HashMap::new();
        for index in 0..=MAX_ACTIVE_UPLOAD_JOBS {
            let path = format!(r"C:\Media\clip-{index}.mp4");
            let record = UploadRecord {
                token: DurableUploadToken {
                    account_key: owner.account_key.clone(),
                    account_generation: owner.account_generation,
                    upload_generation: UploadGeneration::new(index as u64 + 1),
                    local_clip_id: LocalClipId::new(format!("local-{index}")).unwrap(),
                    source_path: ClipPathIdentity::from_text(&path).unwrap(),
                },
                client_clip_id: None,
                path,
                visibility: "private".into(),
                phase: UploadPhase::Completed,
                upload_status: "uploaded_private".into(),
                received_size_bytes: 0,
                file_size_bytes: 0,
                remote_clip_id: None,
                remote_url: None,
                error: None,
                local_deleted: false,
                updated_at_unix: 0,
            };
            remember_live_record(&mut live, record);
        }
        assert_eq!(live.len(), MAX_ACTIVE_UPLOAD_JOBS);
    }

    #[test]
    fn settings_errors_keep_the_shipping_upload_classification() {
        assert_eq!(
            classify_settings_error(SettingsTransactionError::AccountGenerationExhausted).kind(),
            UploadRecordErrorKind::AccountChanged
        );
        assert_eq!(
            classify_settings_error(SettingsTransactionError::StaleCloudRecord).kind(),
            UploadRecordErrorKind::Superseded
        );
        assert_eq!(
            classify_settings_error(SettingsTransactionError::StaleRevision {
                expected: SettingsRevision::INITIAL,
                current: SettingsRevision::INITIAL,
            })
            .kind(),
            UploadRecordErrorKind::Contended
        );
    }
}
