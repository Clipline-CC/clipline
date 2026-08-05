use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use clipline_games::osu::{
    OsuAccessToken, OsuAccountService, OsuAccountServiceError, OsuAccountTestFuture,
    OsuAccountTestPort, OsuClientSecret, OsuCredentialPort, OsuCredentialPortError, OsuSaveRequest,
    OsuSettingsPort, OsuSettingsPortError, MAX_OSU_ACCESS_TOKEN_BYTES, MAX_OSU_CLIENT_SECRET_BYTES,
};
use clipline_games::osu_http::{
    OsuCancellationFuture, OsuHttpConfig, OsuHttpOwner, OsuRecentFetch, OsuRequestFence,
};
use clipline_settings::{
    OsuAccountGeneration, OsuApiSettings, OsuProfileCas, SettingsProfile, SettingsStore,
};
use clipline_test_utils::TestDir;
use zeroize::Zeroizing;

static_assertions::assert_not_impl_any!(OsuClientSecret: Clone, serde::Serialize);
static_assertions::assert_not_impl_any!(OsuAccessToken: Clone, serde::Serialize);

#[derive(Clone, Default)]
struct Fixture {
    events: Arc<Mutex<Vec<&'static str>>>,
    credentials: Arc<Mutex<BTreeMap<String, Zeroizing<String>>>>,
    profile: Arc<Mutex<OsuApiSettings>>,
    fail_cas_calls: Arc<Mutex<Vec<usize>>>,
    cas_calls: Arc<AtomicUsize>,
    fail_write: Arc<Mutex<bool>>,
    fail_delete: Arc<Mutex<bool>>,
}

impl OsuCredentialPort for Fixture {
    fn read(&self, target: &str) -> Result<Option<OsuClientSecret>, OsuCredentialPortError> {
        self.events.lock().unwrap().push("credential.read");
        self.credentials.lock().unwrap().get(target).map_or_else(
            || Ok(None),
            |secret| {
                OsuClientSecret::new(secret.as_str().to_owned())
                    .map(Some)
                    .map_err(|_| OsuCredentialPortError::Unavailable)
            },
        )
    }

    fn write(
        &self,
        target: &str,
        _username: &str,
        secret: &OsuClientSecret,
    ) -> Result<(), OsuCredentialPortError> {
        self.events.lock().unwrap().push("credential.write");
        self.credentials.lock().unwrap().insert(
            target.into(),
            Zeroizing::new(secret.expose_secret().to_owned()),
        );
        if *self.fail_write.lock().unwrap() {
            return Err(OsuCredentialPortError::Unavailable);
        }
        Ok(())
    }

    fn delete(&self, target: &str) -> Result<(), OsuCredentialPortError> {
        self.events.lock().unwrap().push("credential.delete");
        if *self.fail_delete.lock().unwrap() {
            return Err(OsuCredentialPortError::Unavailable);
        }
        self.credentials.lock().unwrap().remove(target);
        Ok(())
    }
}

impl OsuSettingsPort for Fixture {
    fn load(&self) -> Result<OsuApiSettings, OsuSettingsPortError> {
        self.events.lock().unwrap().push("settings.load");
        Ok(self.profile.lock().unwrap().clone())
    }

    fn compare_exchange(
        &self,
        change: OsuProfileCas,
    ) -> Result<OsuApiSettings, OsuSettingsPortError> {
        self.events.lock().unwrap().push("settings.cas");
        let call = self.cas_calls.fetch_add(1, Ordering::SeqCst) + 1;
        if self.fail_cas_calls.lock().unwrap().contains(&call) {
            return Err(OsuSettingsPortError::Stale);
        }
        let mut current = self.profile.lock().unwrap();
        if *current != change.expected {
            return Err(OsuSettingsPortError::Stale);
        }
        *current = change.replacement;
        Ok(current.clone())
    }
}

fn configured_profile(client_id: &str, user: &str, secret: &str) -> (Fixture, String) {
    let fixture = Fixture::default();
    let target = clipline_settings::osu::osu_credential_target(client_id, user);
    fixture
        .credentials
        .lock()
        .unwrap()
        .insert(target.clone(), Zeroizing::new(secret.to_owned()));
    *fixture.profile.lock().unwrap() = OsuApiSettings {
        client_id: Some(client_id.into()),
        user: Some(user.into()),
        credential_target: Some(target.clone()),
        ..OsuApiSettings::default()
    };
    (fixture, target)
}

#[derive(Clone, Copy)]
struct CurrentFence;

impl OsuRequestFence for CurrentFence {
    fn is_current(&self, _owner: OsuHttpOwner) -> bool {
        true
    }

    fn cancelled<'a>(&'a self, _owner: OsuHttpOwner) -> OsuCancellationFuture<'a> {
        Box::pin(std::future::pending())
    }
}

struct SuccessfulTestPort;

impl OsuAccountTestPort for SuccessfulTestPort {
    fn test<'a>(
        &'a self,
        config: OsuHttpConfig,
        _fence: &'a dyn OsuRequestFence,
    ) -> OsuAccountTestFuture<'a> {
        Box::pin(async move {
            Ok(OsuRecentFetch {
                owner: config.owner(),
                user_id: "3426414".into(),
                scores: Vec::new(),
                failed_count: 7,
                started_at_count: 8,
                ended_at_count: 9,
                pagination_ceiling_reached: true,
                username: Some("Dain".into()),
            })
        })
    }
}

struct ReplacingTestPort {
    fixture: Fixture,
}

impl OsuAccountTestPort for ReplacingTestPort {
    fn test<'a>(
        &'a self,
        config: OsuHttpConfig,
        _fence: &'a dyn OsuRequestFence,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<OsuRecentFetch, clipline_games::osu_http::OsuHttpError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let mut profile = self.fixture.profile.lock().unwrap();
            profile.account_generation = profile.account_generation.checked_next().unwrap();
            profile.user = Some("replacement".into());
            drop(profile);
            Ok(OsuRecentFetch {
                owner: config.owner(),
                user_id: "3426414".into(),
                scores: Vec::new(),
                failed_count: 0,
                started_at_count: 0,
                ended_at_count: 0,
                pagination_ceiling_reached: false,
                username: Some("Dain".into()),
            })
        })
    }
}

#[test]
fn secret_owners_are_bounded_and_redacted() {
    let client = OsuClientSecret::new("super-secret".into()).unwrap();
    let token = OsuAccessToken::new("access-token".into()).unwrap();
    assert_eq!(client.expose_secret(), "super-secret");
    assert_eq!(token.expose_secret(), "access-token");
    assert_eq!(format!("{client:?}"), "OsuClientSecret([REDACTED])");
    assert_eq!(format!("{token:?}"), "OsuAccessToken([REDACTED])");
    assert!(OsuClientSecret::new(String::new()).is_err());
    assert!(OsuClientSecret::new("x".repeat(MAX_OSU_CLIENT_SECRET_BYTES + 1)).is_err());
    assert!(OsuAccessToken::new("x".repeat(MAX_OSU_ACCESS_TOKEN_BYTES + 1)).is_err());
    let decoded: OsuClientSecret = serde_json::from_str("\"from-json\"").unwrap();
    assert_eq!(decoded.expose_secret(), "from-json");
}

#[test]
fn save_writes_the_credential_before_exact_profile_cas_and_advances_generation() {
    let fixture = Fixture::default();
    let service = OsuAccountService::new(fixture.clone(), fixture.clone());
    let status = service
        .save(OsuSaveRequest {
            client_id: "12345".into(),
            user: "Dain".into(),
            client_secret: Some(OsuClientSecret::new("new-secret".into()).unwrap()),
        })
        .unwrap();

    let profile = fixture.profile.lock().unwrap().clone();
    assert_eq!(profile.account_generation.get(), 2);
    assert_eq!(status.account_generation, profile.account_generation);
    assert!(status.configured && status.secret_present);
    assert_eq!(status.client_id.as_deref(), Some("12345"));
    assert_eq!(status.user.as_deref(), Some("Dain"));
    assert_eq!(
        *fixture.events.lock().unwrap(),
        [
            "settings.load",
            "credential.read",
            "settings.cas",
            "credential.write",
            "settings.cas",
            "credential.read"
        ]
    );
}

#[test]
fn failed_save_preserves_the_active_credential_and_deletes_only_the_candidate() {
    let (fixture, target) = configured_profile("12345", "Dain", "old-secret");
    *fixture.fail_cas_calls.lock().unwrap() = vec![2];
    let service = OsuAccountService::new(fixture.clone(), fixture.clone());

    let error = service
        .save(OsuSaveRequest {
            client_id: "12345".into(),
            user: "Dain".into(),
            client_secret: Some(OsuClientSecret::new("new-secret".into()).unwrap()),
        })
        .unwrap_err();

    assert_eq!(error, OsuAccountServiceError::StaleProfile);
    assert_eq!(
        fixture.credentials.lock().unwrap()[&target].as_str(),
        "old-secret"
    );
    assert_eq!(fixture.profile.lock().unwrap().account_generation.get(), 1);
    assert_eq!(
        *fixture.events.lock().unwrap(),
        [
            "settings.load",
            "credential.read",
            "settings.cas",
            "credential.write",
            "settings.cas",
            "settings.load",
            "settings.load",
            "credential.delete",
            "settings.cas"
        ]
    );
}

#[test]
fn failed_save_deletes_a_new_unreferenced_credential() {
    let fixture = Fixture::default();
    *fixture.fail_cas_calls.lock().unwrap() = vec![2];
    let service = OsuAccountService::new(fixture.clone(), fixture.clone());
    assert_eq!(
        service
            .save(OsuSaveRequest {
                client_id: "12345".into(),
                user: "Dain".into(),
                client_secret: Some(OsuClientSecret::new("new-secret".into()).unwrap()),
            })
            .unwrap_err(),
        OsuAccountServiceError::StaleProfile
    );
    assert!(fixture.credentials.lock().unwrap().is_empty());
}

#[test]
fn failed_candidate_delete_durably_schedules_that_unreferenced_target() {
    let fixture = Fixture::default();
    *fixture.fail_cas_calls.lock().unwrap() = vec![2];
    *fixture.fail_delete.lock().unwrap() = true;
    let service = OsuAccountService::new(fixture.clone(), fixture.clone());
    let error = service
        .save(OsuSaveRequest {
            client_id: "12345".into(),
            user: "Dain".into(),
            client_secret: Some(OsuClientSecret::new("new-secret".into()).unwrap()),
        })
        .unwrap_err();

    assert_eq!(error, OsuAccountServiceError::StaleProfile);
    let target = fixture
        .profile
        .lock()
        .unwrap()
        .credential_cleanup_targets
        .first()
        .expect("failed candidate is scheduled")
        .clone();
    assert_eq!(
        fixture
            .profile
            .lock()
            .unwrap()
            .credential_cleanup_targets
            .as_slice(),
        std::slice::from_ref(&target)
    );
    assert!(!error.to_string().contains(&target));
    assert_eq!(
        fixture
            .credentials
            .lock()
            .unwrap()
            .get(&target)
            .expect("failed delete leaves candidate scheduled")
            .as_str(),
        "new-secret"
    );
}

#[test]
fn ambiguous_write_failure_is_cleaned_from_its_durable_reservation() {
    let fixture = Fixture::default();
    *fixture.fail_write.lock().unwrap() = true;
    let service = OsuAccountService::new(fixture.clone(), fixture.clone());

    let error = service
        .save(OsuSaveRequest {
            client_id: "12345".into(),
            user: "Dain".into(),
            client_secret: Some(OsuClientSecret::new("partially-written".into()).unwrap()),
        })
        .unwrap_err();

    assert_eq!(error, OsuAccountServiceError::CredentialWrite);
    assert!(fixture.credentials.lock().unwrap().is_empty());
    let profile = fixture.profile.lock().unwrap().clone();
    assert_eq!(profile.account_generation.get(), 1);
    assert!(profile.credential_cleanup_targets.is_empty());
}

#[test]
fn ambiguous_write_and_delete_failure_remains_durably_scheduled() {
    let fixture = Fixture::default();
    *fixture.fail_write.lock().unwrap() = true;
    *fixture.fail_delete.lock().unwrap() = true;
    let service = OsuAccountService::new(fixture.clone(), fixture.clone());

    let error = service
        .save(OsuSaveRequest {
            client_id: "12345".into(),
            user: "Dain".into(),
            client_secret: Some(OsuClientSecret::new("partially-written".into()).unwrap()),
        })
        .unwrap_err();

    assert_eq!(error, OsuAccountServiceError::CredentialWrite);
    let profile = fixture.profile.lock().unwrap().clone();
    assert_eq!(profile.credential_cleanup_targets.len(), 1);
    let candidate = &profile.credential_cleanup_targets[0];
    assert!(fixture.credentials.lock().unwrap().contains_key(candidate));
    assert!(!error.to_string().contains(candidate));
}

#[test]
fn omitted_secret_is_copied_to_the_new_generation_without_plaintext_ownership() {
    let (fixture, old_target) = configured_profile("12345", "Dain", "old-secret");
    let service = OsuAccountService::new(fixture.clone(), fixture.clone());

    let status = service
        .save(OsuSaveRequest {
            client_id: "12345".into(),
            user: "Dain".into(),
            client_secret: None,
        })
        .unwrap();

    assert!(status.configured);
    let profile = fixture.profile.lock().unwrap().clone();
    let target = profile.credential_target.expect("generation target");
    assert_ne!(target, old_target);
    assert_eq!(
        fixture.credentials.lock().unwrap()[&target].as_str(),
        "old-secret"
    );
    assert!(!fixture
        .credentials
        .lock()
        .unwrap()
        .contains_key(&old_target));
}

#[test]
fn disconnect_commits_the_generation_and_cleanup_owner_before_deletion() {
    let (fixture, target) = configured_profile("12345", "Dain", "secret");
    let service = OsuAccountService::new(fixture.clone(), fixture.clone());

    let status = service.disconnect().unwrap();

    assert_eq!(
        status.account_generation,
        OsuAccountGeneration::new(2).unwrap()
    );
    assert!(!status.configured);
    assert!(!fixture.credentials.lock().unwrap().contains_key(&target));
    let profile = fixture.profile.lock().unwrap().clone();
    assert!(profile.client_id.is_none());
    assert!(profile.credential_cleanup_targets.is_empty());
    let events = fixture.events.lock().unwrap().clone();
    let first_cas = events
        .iter()
        .position(|event| *event == "settings.cas")
        .unwrap();
    let delete = events
        .iter()
        .position(|event| *event == "credential.delete")
        .unwrap();
    assert!(
        first_cas < delete,
        "profile must release the target before deletion"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| **event == "settings.cas")
            .count(),
        2,
        "disconnect plus exact cleanup reconciliation"
    );
}

#[test]
fn failed_cleanup_remains_exactly_scheduled_without_leaking_its_target() {
    let (fixture, target) = configured_profile("12345", "Dain", "secret");
    *fixture.fail_delete.lock().unwrap() = true;
    let service = OsuAccountService::new(fixture.clone(), fixture.clone());

    let error = service.disconnect().unwrap_err();

    assert_eq!(error, OsuAccountServiceError::CredentialDelete);
    let profile = fixture.profile.lock().unwrap().clone();
    assert_eq!(
        profile.credential_cleanup_targets.as_slice(),
        std::slice::from_ref(&target)
    );
    assert!(!error.to_string().contains(&target));
    assert!(!format!("{error:?}").contains(&target));
}

#[test]
fn generation_exhaustion_is_rejected_before_any_credential_side_effect() {
    let fixture = Fixture::default();
    fixture.profile.lock().unwrap().account_generation =
        OsuAccountGeneration::new(u64::MAX).unwrap();
    let service = OsuAccountService::new(fixture.clone(), fixture.clone());

    assert_eq!(
        service
            .save(OsuSaveRequest {
                client_id: "12345".into(),
                user: "Dain".into(),
                client_secret: Some(OsuClientSecret::new("secret".into()).unwrap()),
            })
            .unwrap_err(),
        OsuAccountServiceError::GenerationExhausted
    );
    assert_eq!(*fixture.events.lock().unwrap(), ["settings.load"]);
}

#[test]
fn missing_reusable_secret_fails_before_write_or_profile_cas() {
    let fixture = Fixture::default();
    *fixture.profile.lock().unwrap() = OsuApiSettings {
        client_id: Some("12345".into()),
        user: Some("Dain".into()),
        credential_target: Some(clipline_settings::osu::osu_credential_target(
            "12345", "Dain",
        )),
        ..OsuApiSettings::default()
    };
    let service = OsuAccountService::new(fixture.clone(), fixture.clone());

    assert_eq!(
        service
            .save(OsuSaveRequest {
                client_id: "12345".into(),
                user: "Dain".into(),
                client_secret: None,
            })
            .unwrap_err(),
        OsuAccountServiceError::MissingSecret
    );
    assert_eq!(
        *fixture.events.lock().unwrap(),
        ["settings.load", "credential.read", "credential.read"]
    );
}

#[test]
fn concurrent_saves_serialize_into_distinct_durable_generations() {
    let fixture = Fixture::default();
    let service = Arc::new(OsuAccountService::new(fixture.clone(), fixture.clone()));
    let first = {
        let service = Arc::clone(&service);
        std::thread::spawn(move || {
            service.save(OsuSaveRequest {
                client_id: "12345".into(),
                user: "Dain-A".into(),
                client_secret: Some(OsuClientSecret::new("secret-a".into()).unwrap()),
            })
        })
    };
    let second = {
        let service = Arc::clone(&service);
        std::thread::spawn(move || {
            service.save(OsuSaveRequest {
                client_id: "12345".into(),
                user: "Dain-B".into(),
                client_secret: Some(OsuClientSecret::new("secret-b".into()).unwrap()),
            })
        })
    };

    first.join().unwrap().unwrap();
    second.join().unwrap().unwrap();
    let profile = fixture.profile.lock().unwrap().clone();
    assert_eq!(profile.account_generation.get(), 3);
    let active = profile.credential_target.expect("active generation target");
    let credentials = fixture.credentials.lock().unwrap();
    assert_eq!(credentials.len(), 1);
    assert!(credentials.contains_key(&active));
}

#[test]
fn independent_services_share_the_process_operation_gate() {
    let fixture = Fixture::default();
    let first_service = OsuAccountService::new(fixture.clone(), fixture.clone());
    let second_service = OsuAccountService::new(fixture.clone(), fixture.clone());
    let first = std::thread::spawn(move || {
        first_service.save(OsuSaveRequest {
            client_id: "12345".into(),
            user: "Dain-A".into(),
            client_secret: Some(OsuClientSecret::new("secret-a".into()).unwrap()),
        })
    });
    let second = std::thread::spawn(move || {
        second_service.save(OsuSaveRequest {
            client_id: "12345".into(),
            user: "Dain-B".into(),
            client_secret: Some(OsuClientSecret::new("secret-b".into()).unwrap()),
        })
    });

    let results = [first.join().unwrap(), second.join().unwrap()];
    assert!(results.into_iter().all(|result| result.is_ok()));
    let profile = fixture.profile.lock().unwrap().clone();
    assert_eq!(profile.account_generation.get(), 3);
    let active = profile.credential_target.expect("winning candidate");
    let credentials = fixture.credentials.lock().unwrap();
    assert_eq!(credentials.len(), 1);
    assert!(credentials.contains_key(&active));
}

#[test]
fn independently_opened_settings_ports_refresh_the_current_durable_profile() {
    let dir = TestDir::new("clipline-games", "osu-independent-settings-ports");
    let profile = SettingsProfile::isolated(dir.path());
    let first_store = SettingsStore::open(profile.clone());
    let second_store = SettingsStore::open(profile);
    let credentials = Fixture::default();
    let first = OsuAccountService::new(credentials.clone(), first_store);
    let second = OsuAccountService::new(credentials.clone(), second_store.clone());

    first
        .save(OsuSaveRequest {
            client_id: "12345".into(),
            user: "Dain-A".into(),
            client_secret: Some(OsuClientSecret::new("secret-a".into()).unwrap()),
        })
        .unwrap();
    second
        .save(OsuSaveRequest {
            client_id: "12345".into(),
            user: "Dain-B".into(),
            client_secret: Some(OsuClientSecret::new("secret-b".into()).unwrap()),
        })
        .unwrap();

    let current = second_store.current_osu_profile().unwrap();
    assert_eq!(current.account_generation.get(), 3);
    let active = current.credential_target.expect("current target");
    let stored = credentials.credentials.lock().unwrap();
    assert_eq!(stored.len(), 1);
    assert!(stored.contains_key(&active));
}

#[test]
fn save_request_debug_never_contains_the_secret() {
    let request = OsuSaveRequest {
        client_id: "12345".into(),
        user: "Dain".into(),
        client_secret: Some(OsuClientSecret::new("never-print-me".into()).unwrap()),
    };
    let debug = format!("{request:?}");
    assert!(!debug.contains("never-print-me"));
    assert!(debug.contains("[REDACTED]"));
}

#[tokio::test]
async fn account_test_advances_owner_after_http_and_migrates_the_secret() {
    let (fixture, old_target) = configured_profile("12345", "Dain", "secret");
    let service = OsuAccountService::new(fixture.clone(), fixture.clone());

    let result = service
        .test(&SuccessfulTestPort, &CurrentFence)
        .await
        .unwrap();

    assert_eq!(result.status.account_generation.get(), 2);
    assert_eq!(result.status.user.as_deref(), Some("3426414"));
    assert_eq!(result.status.username.as_deref(), Some("Dain"));
    assert_eq!(result.score_count, 0);
    assert_eq!(result.failed_count, 7);
    assert_eq!(result.started_at_count, 8);
    assert_eq!(result.ended_at_count, 9);
    assert!(result.pagination_ceiling_reached);
    let profile = fixture.profile.lock().unwrap().clone();
    let new_target = profile.credential_target.expect("tested target");
    assert_ne!(new_target, old_target);
    let credentials = fixture.credentials.lock().unwrap();
    assert_eq!(credentials.len(), 1);
    assert_eq!(credentials[&new_target].as_str(), "secret");
}

#[tokio::test]
async fn account_replacement_during_http_rejects_stale_test_without_credential_write() {
    let (fixture, active_target) = configured_profile("12345", "Dain", "secret");
    let service = OsuAccountService::new(fixture.clone(), fixture.clone());
    let port = ReplacingTestPort {
        fixture: fixture.clone(),
    };
    let writes_before = fixture
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| **event == "credential.write")
        .count();

    let error = service.test(&port, &CurrentFence).await.unwrap_err();

    assert_eq!(error, OsuAccountServiceError::StaleProfile);
    assert_eq!(fixture.credentials.lock().unwrap().len(), 1);
    assert!(fixture
        .credentials
        .lock()
        .unwrap()
        .contains_key(&active_target));
    let writes_after = fixture
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| **event == "credential.write")
        .count();
    assert_eq!(writes_after, writes_before);
}
