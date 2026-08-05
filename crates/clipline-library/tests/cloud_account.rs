use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use clipline_library::ports::{CloudCredential, PortError};
use clipline_library::settings::SettingsCloudAccountPort;
use clipline_library::{
    CloudAccountCancellation, CloudAccountFuture, CloudAccountInvalidationPort,
    CloudAccountMutationPort, CloudAccountService, CloudAccountServiceError, CloudAccountState,
    CloudAccountTransport, CloudAuthenticatedAccount, CloudAuthenticationRequest,
    CloudConnectRequest, CloudConnectedProfile, CloudCredentialMutationPort, CloudPassword,
    CloudServiceAccount, NeverCancelCloudAccount,
};
use clipline_settings::{SettingsProfile, SettingsStore};
use clipline_test_utils::TestDir;

type AuthenticationRequests = Arc<Mutex<Vec<(String, Option<String>)>>>;

#[derive(Clone, Default)]
struct FakeCredentials {
    inner: Arc<Mutex<CredentialState>>,
}

#[derive(Default)]
struct CredentialState {
    values: BTreeMap<String, String>,
    events: Vec<String>,
    fail_write: bool,
    fail_delete: bool,
}

impl CloudCredentialMutationPort for FakeCredentials {
    fn read(&self, target: &str) -> Result<Option<CloudCredential>, PortError> {
        let mut inner = self.inner.lock().unwrap();
        inner.events.push(format!("read:{target}"));
        Ok(inner.values.get(target).cloned().map(CloudCredential::new))
    }

    fn write(
        &self,
        target: &str,
        _username: &str,
        credential: &CloudCredential,
    ) -> Result<(), PortError> {
        let mut inner = self.inner.lock().unwrap();
        inner.events.push(format!("write:{target}"));
        if inner.fail_write {
            return Err(PortError::new("injected credential write failure"));
        }
        inner
            .values
            .insert(target.to_owned(), credential.expose().to_owned());
        Ok(())
    }

    fn delete_if_present(&self, target: &str) -> Result<(), PortError> {
        let mut inner = self.inner.lock().unwrap();
        inner.events.push(format!("delete:{target}"));
        if inner.fail_delete {
            return Err(PortError::new("injected credential delete failure"));
        }
        inner.values.remove(target);
        Ok(())
    }
}

#[derive(Clone)]
struct FakeTransport {
    events: Arc<Mutex<Vec<String>>>,
    requests: AuthenticationRequests,
    failure: bool,
}

impl CloudAccountTransport for FakeTransport {
    fn authenticate<'a>(
        &'a self,
        request: CloudAuthenticationRequest,
        cancellation: &'a dyn CloudAccountCancellation,
    ) -> CloudAccountFuture<'a> {
        Box::pin(async move {
            self.events.lock().unwrap().push("authenticate".into());
            self.requests.lock().unwrap().push((
                request.device_name.clone(),
                request.default_visibility.clone(),
            ));
            assert_eq!(request.username, "dain");
            assert_eq!(request.password.expose(), "password-sentinel");
            if cancellation.is_canceled() {
                return Err(clipline_library::protocol::CloudProtocolError::Canceled);
            }
            if self.failure {
                return Err(clipline_library::protocol::CloudProtocolError::Http(
                    "injected transport failure".into(),
                ));
            }
            Ok(CloudAuthenticatedAccount {
                host_url: request.base.as_str().trim_end_matches('/').into(),
                public_url: "https://public.example".into(),
                user_id: "user-1".into(),
                username: "dain".into(),
                display_name: Some("Dain".into()),
                credential: CloudCredential::new("token-sentinel"),
            })
        })
    }
}

#[derive(Clone)]
struct InspectInvalidation {
    store: SettingsStore,
    events: Arc<Mutex<Vec<String>>>,
}

impl CloudAccountInvalidationPort for InspectInvalidation {
    fn account_changed(
        &self,
        _previous: Option<&CloudServiceAccount>,
        current: Option<&CloudServiceAccount>,
    ) {
        let durable = self.store.current_cloud_account().unwrap();
        self.events.lock().unwrap().push(format!(
            "invalidate:{}:{}",
            current.is_some(),
            durable.document.cloud.connected()
        ));
    }
}

fn request() -> CloudConnectRequest {
    CloudConnectRequest {
        host_url: "https://clips.example/base/".into(),
        username: " dain ".into(),
        password: CloudPassword::new("password-sentinel".into()).unwrap(),
        device_name: Some(" Clipline Desktop ".into()),
        plain_http_confirmed: false,
        default_visibility: Some("unlisted".into()),
    }
}

struct Fixture {
    _dir: TestDir,
    store: SettingsStore,
    credentials: FakeCredentials,
    events: Arc<Mutex<Vec<String>>>,
    requests: AuthenticationRequests,
}

struct AlreadyCanceled;

impl CloudAccountCancellation for AlreadyCanceled {
    fn is_canceled(&self) -> bool {
        true
    }
}

impl Fixture {
    fn new(case: &str) -> Self {
        let dir = TestDir::new("clipline-library-cloud-account", case);
        let store = SettingsStore::open(SettingsProfile::isolated(dir.path()));
        Self {
            _dir: dir,
            store,
            credentials: FakeCredentials::default(),
            events: Arc::new(Mutex::new(Vec::new())),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn service(
        &self,
        transport_failure: bool,
    ) -> CloudAccountService<
        SettingsCloudAccountPort,
        FakeCredentials,
        FakeTransport,
        InspectInvalidation,
    > {
        CloudAccountService::new(
            SettingsCloudAccountPort::new(self.store.clone()),
            self.credentials.clone(),
            FakeTransport {
                events: self.events.clone(),
                requests: self.requests.clone(),
                failure: transport_failure,
            },
            InspectInvalidation {
                store: self.store.clone(),
                events: self.events.clone(),
            },
        )
    }
}

#[derive(Clone)]
struct CommitThenFailSettings {
    inner: SettingsCloudAccountPort,
}

impl CloudAccountMutationPort for CommitThenFailSettings {
    fn load(&self) -> Result<CloudAccountState, PortError> {
        self.inner.load()
    }

    fn reserve_credential(
        &self,
        expected: &CloudAccountState,
        target: String,
    ) -> Result<CloudAccountState, PortError> {
        self.inner.reserve_credential(expected, target)
    }

    fn commit_connect(
        &self,
        expected: &CloudAccountState,
        connected: CloudConnectedProfile,
        default_visibility: Option<String>,
    ) -> Result<CloudAccountState, PortError> {
        let _committed = self
            .inner
            .commit_connect(expected, connected, default_visibility)?;
        Err(PortError::new(
            "injected acknowledgement failure after durable commit",
        ))
    }

    fn commit_disconnect(
        &self,
        expected: &CloudAccountState,
    ) -> Result<CloudAccountState, PortError> {
        self.inner.commit_disconnect(expected)
    }

    fn reconcile_cleanup(
        &self,
        expected: &CloudAccountState,
        deleted_targets: &[String],
    ) -> Result<CloudAccountState, PortError> {
        self.inner.reconcile_cleanup(expected, deleted_targets)
    }
}

#[derive(Clone)]
struct DisconnectThenFailSettings {
    inner: SettingsCloudAccountPort,
}

impl CloudAccountMutationPort for DisconnectThenFailSettings {
    fn load(&self) -> Result<CloudAccountState, PortError> {
        self.inner.load()
    }

    fn reserve_credential(
        &self,
        expected: &CloudAccountState,
        target: String,
    ) -> Result<CloudAccountState, PortError> {
        self.inner.reserve_credential(expected, target)
    }

    fn commit_connect(
        &self,
        expected: &CloudAccountState,
        connected: CloudConnectedProfile,
        default_visibility: Option<String>,
    ) -> Result<CloudAccountState, PortError> {
        self.inner
            .commit_connect(expected, connected, default_visibility)
    }

    fn commit_disconnect(
        &self,
        expected: &CloudAccountState,
    ) -> Result<CloudAccountState, PortError> {
        let _committed = self.inner.commit_disconnect(expected)?;
        Err(PortError::new(
            "injected acknowledgement failure after durable disconnect",
        ))
    }

    fn reconcile_cleanup(
        &self,
        expected: &CloudAccountState,
        deleted_targets: &[String],
    ) -> Result<CloudAccountState, PortError> {
        self.inner.reconcile_cleanup(expected, deleted_targets)
    }
}

#[derive(Default)]
struct WriteGateState {
    entered: bool,
    released: bool,
}

#[derive(Clone, Default)]
struct BlockingFirstWriteCredentials {
    inner: FakeCredentials,
    gate: Arc<(Mutex<WriteGateState>, Condvar)>,
}

impl BlockingFirstWriteCredentials {
    fn wait_until_entered(&self) {
        let (lock, wake) = &*self.gate;
        let mut state = lock.lock().unwrap();
        while !state.entered {
            state = wake.wait(state).unwrap();
        }
    }

    fn release(&self) {
        let (lock, wake) = &*self.gate;
        lock.lock().unwrap().released = true;
        wake.notify_all();
    }
}

impl CloudCredentialMutationPort for BlockingFirstWriteCredentials {
    fn read(&self, target: &str) -> Result<Option<CloudCredential>, PortError> {
        self.inner.read(target)
    }

    fn write(
        &self,
        target: &str,
        username: &str,
        credential: &CloudCredential,
    ) -> Result<(), PortError> {
        let (lock, wake) = &*self.gate;
        let mut state = lock.lock().unwrap();
        if !state.entered {
            state.entered = true;
            wake.notify_all();
            while !state.released {
                state = wake.wait(state).unwrap();
            }
        }
        drop(state);
        self.inner.write(target, username, credential)
    }

    fn delete_if_present(&self, target: &str) -> Result<(), PortError> {
        self.inner.delete_if_present(target)
    }
}

#[derive(Default)]
struct ToggleCancellation {
    canceled: AtomicBool,
    checks: AtomicUsize,
}

impl ToggleCancellation {
    fn cancel(&self) {
        self.canceled.store(true, Ordering::Release);
    }
}

impl CloudAccountCancellation for ToggleCancellation {
    fn is_canceled(&self) -> bool {
        self.checks.fetch_add(1, Ordering::AcqRel);
        self.canceled.load(Ordering::Acquire)
    }
}

#[tokio::test]
async fn connect_publishes_credential_then_exact_account_then_new_owner() {
    let fixture = Fixture::new("connect-success");
    let service = fixture.service(false);

    let status = service
        .connect(request(), &NeverCancelCloudAccount)
        .await
        .unwrap();

    assert!(status.connected);
    assert!(status.token_present);
    assert_eq!(status.host_url, "https://clips.example/base");
    assert_eq!(status.default_visibility, "unlisted");
    let durable = fixture.store.current_cloud_account().unwrap();
    assert!(durable.document.cloud.connected());
    assert!(durable.document.cloud.credential_cleanup_targets.is_empty());
    let target = durable.document.cloud.credential_target.clone().unwrap();
    assert!(target.starts_with("Clipline Cloud:operation:"));
    assert_eq!(
        fixture.credentials.inner.lock().unwrap().values[&target],
        "token-sentinel"
    );
    assert_eq!(
        fixture.events.lock().unwrap().as_slice(),
        ["authenticate", "invalidate:true:true"]
    );
}

#[tokio::test]
async fn ambiguous_commit_error_preserves_the_durably_connected_credential() {
    let fixture = Fixture::new("ambiguous-commit");
    let settings = SettingsCloudAccountPort::new(fixture.store.clone());
    let service = CloudAccountService::new(
        CommitThenFailSettings { inner: settings },
        fixture.credentials.clone(),
        FakeTransport {
            events: fixture.events.clone(),
            requests: fixture.requests.clone(),
            failure: false,
        },
        InspectInvalidation {
            store: fixture.store.clone(),
            events: fixture.events.clone(),
        },
    );

    let status = service
        .connect(request(), &NeverCancelCloudAccount)
        .await
        .unwrap();

    assert!(status.connected);
    assert!(status.token_present);
    let durable = fixture.store.current_cloud_account().unwrap();
    assert!(durable.document.cloud.connected());
    let target = durable.document.cloud.credential_target.clone().unwrap();
    assert!(durable.document.cloud.credential_cleanup_targets.is_empty());
    assert_eq!(
        fixture.credentials.inner.lock().unwrap().values[&target],
        "token-sentinel"
    );
    assert!(!fixture
        .credentials
        .inner
        .lock()
        .unwrap()
        .events
        .iter()
        .any(|event| event == &format!("delete:{target}")));
}

#[tokio::test]
async fn omitted_visibility_and_blank_device_keep_shipping_connect_defaults() {
    let fixture = Fixture::new("connect-defaults");
    let mut connect = request();
    connect.device_name = Some("   ".into());
    connect.default_visibility = None;

    let status = fixture
        .service(false)
        .connect(connect, &NeverCancelCloudAccount)
        .await
        .unwrap();

    assert_eq!(status.default_visibility, "private");
    assert_eq!(
        fixture.requests.lock().unwrap().as_slice(),
        [("Clipline Desktop".into(), Some("private".into()))]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancellation_while_waiting_for_account_serialization_prevents_a_write() {
    let fixture = Fixture::new("canceled-while-queued");
    let credentials = BlockingFirstWriteCredentials::default();
    let first_service = CloudAccountService::new(
        SettingsCloudAccountPort::new(fixture.store.clone()),
        credentials.clone(),
        FakeTransport {
            events: fixture.events.clone(),
            requests: fixture.requests.clone(),
            failure: false,
        },
        InspectInvalidation {
            store: fixture.store.clone(),
            events: fixture.events.clone(),
        },
    );
    let first = tokio::spawn(async move {
        first_service
            .connect(request(), &NeverCancelCloudAccount)
            .await
    });
    credentials.wait_until_entered();

    let cancellation = Arc::new(ToggleCancellation::default());
    let second_cancellation = Arc::clone(&cancellation);
    let second_service = CloudAccountService::new(
        SettingsCloudAccountPort::new(fixture.store.clone()),
        credentials.clone(),
        FakeTransport {
            events: fixture.events.clone(),
            requests: fixture.requests.clone(),
            failure: false,
        },
        InspectInvalidation {
            store: fixture.store.clone(),
            events: fixture.events.clone(),
        },
    );
    let second = tokio::spawn(async move {
        second_service
            .connect(request(), second_cancellation.as_ref())
            .await
    });
    while cancellation.checks.load(Ordering::Acquire) < 3 {
        tokio::task::yield_now().await;
    }
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    cancellation.cancel();
    credentials.release();

    assert!(first.await.unwrap().unwrap().connected);
    assert_eq!(
        second.await.unwrap().unwrap_err(),
        CloudAccountServiceError::Canceled
    );
    assert_eq!(credentials.inner.inner.lock().unwrap().values.len(), 1);
}

#[tokio::test]
async fn precommit_and_credential_failures_never_publish_a_partial_account() {
    let transport_fixture = Fixture::new("transport-failure");
    let error = transport_fixture
        .service(true)
        .connect(request(), &NeverCancelCloudAccount)
        .await
        .unwrap_err();
    assert_eq!(error, CloudAccountServiceError::Protocol);
    assert!(!transport_fixture
        .store
        .current_cloud_account()
        .unwrap()
        .document
        .cloud
        .connected());
    assert!(transport_fixture
        .credentials
        .inner
        .lock()
        .unwrap()
        .values
        .is_empty());

    let write_fixture = Fixture::new("write-failure");
    write_fixture.credentials.inner.lock().unwrap().fail_write = true;
    let error = write_fixture
        .service(false)
        .connect(request(), &NeverCancelCloudAccount)
        .await
        .unwrap_err();
    assert_eq!(error, CloudAccountServiceError::CredentialWrite);
    let durable = write_fixture.store.current_cloud_account().unwrap();
    assert!(!durable.document.cloud.connected());
    assert!(durable.document.cloud.credential_cleanup_targets.is_empty());
}

#[tokio::test]
async fn password_and_token_are_redacted_from_debug_and_errors() {
    let request = request();
    let debug = format!("{request:?}");
    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains("password-sentinel"));
    assert_eq!(
        format!("{:?}", CloudCredential::new("token-sentinel")),
        "CloudCredential([REDACTED])"
    );
}

#[tokio::test]
async fn cancellation_before_authentication_has_no_side_effects() {
    let fixture = Fixture::new("canceled");
    let error = fixture
        .service(false)
        .connect(request(), &AlreadyCanceled)
        .await
        .unwrap_err();
    assert_eq!(error, CloudAccountServiceError::Canceled);
    assert!(!fixture
        .store
        .current_cloud_account()
        .unwrap()
        .document
        .cloud
        .connected());
    assert!(fixture.events.lock().unwrap().is_empty());
}

#[tokio::test]
async fn disconnect_clears_durable_owner_before_invalidation_and_cleanup() {
    let fixture = Fixture::new("disconnect-order");
    let service = fixture.service(false);
    service
        .connect(request(), &NeverCancelCloudAccount)
        .await
        .unwrap();
    fixture.events.lock().unwrap().clear();

    let status = service.disconnect().unwrap();

    assert!(!status.connected);
    assert!(!status.token_present);
    let durable = fixture.store.current_cloud_account().unwrap();
    assert!(!durable.document.cloud.connected());
    assert!(durable.document.cloud.credential_target.is_none());
    assert!(durable.document.cloud.credential_cleanup_targets.is_empty());
    assert_eq!(
        fixture.events.lock().unwrap().as_slice(),
        ["invalidate:false:false"]
    );
    assert!(fixture.credentials.inner.lock().unwrap().values.is_empty());
}

#[tokio::test]
async fn ambiguous_disconnect_error_still_invalidates_and_cleans_committed_owner() {
    let fixture = Fixture::new("ambiguous-disconnect");
    fixture
        .service(false)
        .connect(request(), &NeverCancelCloudAccount)
        .await
        .unwrap();
    fixture.events.lock().unwrap().clear();
    let service = CloudAccountService::new(
        DisconnectThenFailSettings {
            inner: SettingsCloudAccountPort::new(fixture.store.clone()),
        },
        fixture.credentials.clone(),
        FakeTransport {
            events: fixture.events.clone(),
            requests: fixture.requests.clone(),
            failure: false,
        },
        InspectInvalidation {
            store: fixture.store.clone(),
            events: fixture.events.clone(),
        },
    );

    let status = service.disconnect().unwrap();

    assert!(!status.connected);
    assert!(!status.token_present);
    let durable = fixture.store.current_cloud_account().unwrap();
    assert!(!durable.document.cloud.connected());
    assert!(durable.document.cloud.credential_target.is_none());
    assert!(durable.document.cloud.credential_cleanup_targets.is_empty());
    assert!(fixture.credentials.inner.lock().unwrap().values.is_empty());
    assert_eq!(
        fixture.events.lock().unwrap().as_slice(),
        ["invalidate:false:false"]
    );
}

#[tokio::test]
async fn disconnect_cleanup_failure_remains_durable_and_retryable() {
    let fixture = Fixture::new("disconnect-cleanup-retry");
    let service = fixture.service(false);
    service
        .connect(request(), &NeverCancelCloudAccount)
        .await
        .unwrap();
    fixture.credentials.inner.lock().unwrap().fail_delete = true;

    let status = service.disconnect().unwrap();

    assert!(!status.connected);
    let disconnected = fixture.store.current_cloud_account().unwrap();
    assert_eq!(
        disconnected.document.cloud.credential_cleanup_targets.len(),
        1
    );
    fixture.credentials.inner.lock().unwrap().fail_delete = false;
    let status = service.status().unwrap();
    assert!(!status.connected);
    assert!(fixture
        .store
        .current_cloud_account()
        .unwrap()
        .document
        .cloud
        .credential_cleanup_targets
        .is_empty());
}
