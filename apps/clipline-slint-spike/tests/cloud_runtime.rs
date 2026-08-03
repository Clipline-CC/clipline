use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use clipline_library::cloud::settings::SettingsCloudAccountPort;
use clipline_library::ports::{
    AvatarTransportResult, CloudAccountPort, CloudCredential, CloudCredentialPort,
    CloudProfilePatch, CloudRequestFence, CloudTransport, CloudTransportFuture, PortError,
};
use clipline_library::{
    catalog_result_channel, CatalogAction, CatalogCloudPreferences, CatalogController,
    CatalogEffect, CatalogItemIdentity, CatalogOperationOwner, CatalogResult, CatalogRevision,
    CatalogSource, CloudAccountGeneration, CloudAccountKey, CloudAccountSnapshot,
    CloudCatalogOwner, CloudClipSummary, CloudListTransportRequest, CloudListTransportResponse,
    CloudPageNumber, CloudPageOutcome, CloudProfileTransport, CloudServiceAccount, CloudWorkToken,
    ExpectedResultOwner, ForegroundGeneration, LocalDay, LocalDayResolver, RemoteClipId,
    RequestGeneration, WindowAttachmentGeneration, WindowWorkToken,
};
use clipline_settings::{AppSettings, SettingsProfile, SettingsStore};
use clipline_slint_spike::catalog::{
    CatalogEffectExecutor, CatalogEffectHandler, CatalogResultWake, OwnedCatalogResult,
};
use clipline_slint_spike::cloud::{
    CatalogCloudLifetime, NativeCloudPlatformPort, NativeCloudRuntime,
};
use clipline_test_utils::TestDir;

#[derive(Clone)]
struct FakeAccount {
    account: Arc<Mutex<CloudServiceAccount>>,
}

impl CloudAccountPort for FakeAccount {
    fn snapshot(&self) -> Result<CloudServiceAccount, PortError> {
        Ok(self.account.lock().unwrap().clone())
    }

    fn apply_profile(
        &self,
        expected_key: &CloudAccountKey,
        expected_generation: CloudAccountGeneration,
        patch: CloudProfilePatch,
    ) -> Result<CloudServiceAccount, PortError> {
        let mut account = self.account.lock().unwrap();
        if &account.snapshot.account_key != expected_key
            || account.snapshot.generation != expected_generation
        {
            return Err(PortError::account_changed());
        }
        account.snapshot.user_id = Some(patch.user_id);
        account.snapshot.username = Some(patch.username);
        account.snapshot.display_name = patch.display_name;
        Ok(account.clone())
    }
}

#[derive(Default)]
struct FakeCredential {
    reads: AtomicUsize,
}

impl CloudCredentialPort for FakeCredential {
    fn read(&self, target: &str) -> Result<CloudCredential, PortError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if target != "credential-a" {
            return Err(PortError::new("unexpected credential target"));
        }
        Ok(CloudCredential::new("secret"))
    }
}

enum ListMode {
    Success,
    Failure,
    WaitForCancellation(Mutex<Option<mpsc::Sender<()>>>),
}

struct FakeTransport {
    mode: ListMode,
    requests: Mutex<Vec<CloudListTransportRequest>>,
}

impl FakeTransport {
    fn success() -> Self {
        Self {
            mode: ListMode::Success,
            requests: Mutex::new(Vec::new()),
        }
    }

    fn failure() -> Self {
        Self {
            mode: ListMode::Failure,
            requests: Mutex::new(Vec::new()),
        }
    }

    fn waiting(started: mpsc::Sender<()>) -> Self {
        Self {
            mode: ListMode::WaitForCancellation(Mutex::new(Some(started))),
            requests: Mutex::new(Vec::new()),
        }
    }
}

impl CloudTransport for FakeTransport {
    fn list<'a>(
        &'a self,
        _account: &'a CloudServiceAccount,
        _credential: &'a CloudCredential,
        request: &'a CloudListTransportRequest,
        cancellation: &'a dyn CloudRequestFence,
        token: &'a CloudWorkToken,
    ) -> CloudTransportFuture<'a, CloudListTransportResponse> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(request.clone());
            match &self.mode {
                ListMode::Success => Ok(CloudListTransportResponse {
                    page: request.page,
                    page_size: request.page_size,
                    clips: vec![CloudClipSummary {
                        remote_clip_id: "remote-1".into(),
                        local_clip_id: Some("local-1".into()),
                        title: "Cloud clip".into(),
                        public_url: Some("https://clips.example/clip/remote-1".into()),
                        visibility: "private".into(),
                        status: "ready".into(),
                        updated_at_unix: 9,
                        uploaded_at_unix: Some(8),
                        duration_ms: Some(1_000),
                        file_size_bytes: Some(4_096),
                        source_type: Some("replay".into()),
                    }],
                }),
                ListMode::Failure => Err(PortError::new("server unavailable")),
                ListMode::WaitForCancellation(started) => {
                    if let Some(started) = started.lock().unwrap().take() {
                        let _ = started.send(());
                    }
                    cancellation.cancelled(token).await;
                    Err(PortError::canceled())
                }
            }
        })
    }

    fn profile<'a>(
        &'a self,
        _account: &'a CloudServiceAccount,
        _credential: &'a CloudCredential,
        _cancellation: &'a dyn CloudRequestFence,
        _token: &'a CloudWorkToken,
    ) -> CloudTransportFuture<'a, CloudProfileTransport> {
        Box::pin(async { Err(PortError::new("profile is outside this test")) })
    }

    fn avatar<'a>(
        &'a self,
        _account: &'a CloudServiceAccount,
        _credential: &'a CloudCredential,
        _etag: Option<&'a str>,
        _cancellation: &'a dyn CloudRequestFence,
        _token: &'a CloudWorkToken,
    ) -> CloudTransportFuture<'a, AvatarTransportResult> {
        Box::pin(async { Err(PortError::new("avatar is outside this test")) })
    }
}

struct RejectLocal;

impl CatalogEffectHandler for RejectLocal {
    fn execute(&self, effect: CatalogEffect) -> Result<Option<OwnedCatalogResult>, String> {
        Err(format!("unexpected local effect: {effect:?}"))
    }
}

struct NoopWake;

impl CatalogResultWake for NoopWake {
    fn wake(&self) {}
}

#[derive(Default)]
struct RecordingCloudPlatform {
    opened: Mutex<Vec<(String, String)>>,
    copied: Mutex<Vec<(String, String)>>,
}

impl NativeCloudPlatformPort for RecordingCloudPlatform {
    fn open_browser(&self, url: &str, context: &str) -> Result<(), String> {
        self.opened
            .lock()
            .unwrap()
            .push((url.to_owned(), context.to_owned()));
        Ok(())
    }

    fn copy_text(&self, text: &str, context: &str) -> Result<(), String> {
        self.copied
            .lock()
            .unwrap()
            .push((text.to_owned(), context.to_owned()));
        Ok(())
    }
}

#[derive(Default)]
struct CountingLocal(AtomicUsize);

impl CatalogEffectHandler for CountingLocal {
    fn execute(&self, _effect: CatalogEffect) -> Result<Option<OwnedCatalogResult>, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
}

#[derive(Clone, Copy)]
struct TestDays;

impl LocalDayResolver for TestDays {
    fn today_start_unix(&self) -> u64 {
        10
    }

    fn resolve_day(&self, _timestamp: u64) -> LocalDay {
        LocalDay {
            key: "today".into(),
            label: "Today".into(),
        }
    }
}

fn service_account() -> CloudServiceAccount {
    CloudServiceAccount {
        snapshot: CloudAccountSnapshot {
            account_key: CloudAccountKey::new("account-a").unwrap(),
            generation: CloudAccountGeneration::new(7),
            connected: true,
            host_url: "https://clips.example".into(),
            public_url: Some("https://clips.example".into()),
            username: Some("user".into()),
            display_name: Some("User".into()),
            user_id: Some("user-1".into()),
            default_visibility: "private".into(),
            delete_local_after_upload: false,
            auto_upload_rules: false,
        },
        credential_target: Some("credential-a".into()),
        local_paths_by_clip_id: BTreeMap::from([("local-1".into(), r"C:\Clips\One.mp4".into())]),
    }
}

fn owner() -> CloudCatalogOwner {
    CloudCatalogOwner {
        account_key: CloudAccountKey::new("account-a").unwrap(),
        account_generation: CloudAccountGeneration::new(7),
    }
}

fn token(request: u64) -> CloudWorkToken {
    CloudWorkToken {
        window: WindowWorkToken {
            attachment: WindowAttachmentGeneration::new(3),
            foreground: ForegroundGeneration::new(5),
            request: RequestGeneration::new(request),
        },
        account_key: owner().account_key,
        account_generation: owner().account_generation,
    }
}

fn effect(token: CloudWorkToken) -> CatalogEffect {
    CatalogEffect::RefreshCloud {
        token,
        revision: CatalogRevision::new(11),
        page: CloudPageNumber::new(2).unwrap(),
        query: "  clutch  ".into(),
    }
}

fn make_runtime(
    transport: Arc<FakeTransport>,
) -> (
    NativeCloudRuntime,
    Arc<FakeCredential>,
    Arc<dyn CatalogEffectHandler>,
) {
    let accounts = Arc::new(FakeAccount {
        account: Arc::new(Mutex::new(service_account())),
    });
    let credentials = Arc::new(FakeCredential::default());
    let runtime = NativeCloudRuntime::with_transport(accounts, credentials.clone(), transport)
        .expect("start native cloud runtime");
    runtime
        .attach(
            WindowAttachmentGeneration::new(3),
            ForegroundGeneration::new(5),
            Some(owner()),
        )
        .unwrap();
    let handler = runtime.effect_handler(Arc::new(RejectLocal));
    (runtime, credentials, handler)
}

#[test]
fn exact_refresh_maps_query_page_revision_token_and_failure_owner() {
    let transport = Arc::new(FakeTransport::success());
    let (runtime, _, handler) = make_runtime(transport.clone());
    let exact = token(8);
    let completion = handler.execute(effect(exact.clone())).unwrap().unwrap();
    assert_eq!(
        completion.expected,
        ExpectedResultOwner::Cloud(exact.clone())
    );
    let CatalogResult::CloudPage(completion) = completion.result else {
        panic!("expected Cloud page completion");
    };
    assert_eq!(completion.token, exact);
    assert_eq!(completion.revision, CatalogRevision::new(11));
    assert!(matches!(
        completion.outcome,
        CloudPageOutcome::Page { page, .. } if page == CloudPageNumber::new(2).unwrap()
    ));
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].page, 2);
    assert_eq!(requests[0].query.query.as_deref(), Some("clutch"));
    drop(requests);
    runtime.shutdown().unwrap();

    let (runtime, _, handler) = make_runtime(Arc::new(FakeTransport::failure()));
    let exact = token(9);
    let failure = handler.execute(effect(exact.clone())).unwrap().unwrap();
    let expected_owner = CatalogOperationOwner::CloudRefresh {
        token: exact,
        revision: CatalogRevision::new(11),
        page: CloudPageNumber::new(2).unwrap(),
    };
    assert_eq!(
        failure.expected,
        ExpectedResultOwner::Operation(expected_owner.clone())
    );
    assert!(matches!(
        failure.result,
        CatalogResult::OperationFailed { owner, .. } if owner == expected_owner
    ));
    runtime.shutdown().unwrap();
}

#[test]
fn composite_handler_delegates_local_effects_without_touching_cloud_ports() {
    let accounts = Arc::new(FakeAccount {
        account: Arc::new(Mutex::new(service_account())),
    });
    let credentials = Arc::new(FakeCredential::default());
    let transport = Arc::new(FakeTransport::success());
    let runtime =
        NativeCloudRuntime::with_transport(accounts, credentials.clone(), transport.clone())
            .unwrap();
    let local = Arc::new(CountingLocal::default());
    let handler = runtime.effect_handler(local.clone());
    assert!(handler
        .execute(CatalogEffect::RefreshLocal {
            token: WindowWorkToken {
                attachment: WindowAttachmentGeneration::new(3),
                foreground: ForegroundGeneration::new(5),
                request: RequestGeneration::new(1),
            },
            revision: CatalogRevision::new(1),
        })
        .unwrap()
        .is_none());
    assert_eq!(local.0.load(Ordering::SeqCst), 1);
    assert_eq!(credentials.reads.load(Ordering::SeqCst), 0);
    assert!(transport.requests.lock().unwrap().is_empty());
    runtime.shutdown().unwrap();
}

#[test]
fn public_link_effects_resolve_the_exact_clip_page_and_copy_only_the_issued_url() {
    let accounts = Arc::new(FakeAccount {
        account: Arc::new(Mutex::new(service_account())),
    });
    let platform = Arc::new(RecordingCloudPlatform::default());
    let runtime = NativeCloudRuntime::with_transport_and_platform(
        accounts,
        Arc::new(FakeCredential::default()),
        Arc::new(FakeTransport::success()),
        platform.clone(),
    )
    .unwrap();
    runtime
        .attach(
            WindowAttachmentGeneration::new(3),
            ForegroundGeneration::new(5),
            Some(owner()),
        )
        .unwrap();
    let handler = runtime.effect_handler(Arc::new(RejectLocal));
    let item = CatalogItemIdentity::Cloud {
        account_key: owner().account_key,
        account_generation: owner().account_generation,
        remote_clip_id: clipline_library::RemoteClipId::new("remote-1").unwrap(),
    };

    assert!(handler
        .execute(CatalogEffect::OpenInBrowser {
            token: token(20),
            item: item.clone(),
        })
        .unwrap()
        .is_none());
    let copied = handler
        .execute(CatalogEffect::CopyPublicLink {
            token: token(21),
            item: item.clone(),
            url: "https://public.example/c/server-issued".into(),
        })
        .unwrap()
        .unwrap();
    assert!(matches!(
        copied.result,
        CatalogResult::ForegroundFeedback { message, .. }
            if message == "Cloud public link copied"
    ));

    assert_eq!(
        platform.opened.lock().unwrap().as_slice(),
        &[(
            "https://clips.example/clip/remote-1".into(),
            "cloud clip page".into()
        )]
    );
    assert_eq!(
        platform.copied.lock().unwrap().as_slice(),
        &[(
            "https://public.example/c/server-issued".into(),
            "copy Cloud public link".into()
        )]
    );

    let mut stale = token(22);
    stale.window.foreground = ForegroundGeneration::new(99);
    assert!(handler
        .execute(CatalogEffect::OpenInBrowser {
            token: stale.clone(),
            item: item.clone(),
        })
        .unwrap()
        .is_none());
    assert!(handler
        .execute(CatalogEffect::CopyPublicLink {
            token: stale,
            item: item.clone(),
            url: "https://public.example/c/stale".into(),
        })
        .unwrap()
        .is_none());
    let mut stale_account = token(23);
    stale_account.account_generation = CloudAccountGeneration::new(8);
    let stale_account_item = CatalogItemIdentity::Cloud {
        account_key: stale_account.account_key.clone(),
        account_generation: stale_account.account_generation,
        remote_clip_id: RemoteClipId::new("remote-1").unwrap(),
    };
    assert!(handler
        .execute(CatalogEffect::CopyPublicLink {
            token: stale_account,
            item: stale_account_item,
            url: "https://public.example/c/stale-account".into(),
        })
        .unwrap()
        .is_none());
    runtime.detach();
    assert!(handler
        .execute(CatalogEffect::OpenInBrowser {
            token: token(23),
            item,
        })
        .unwrap()
        .is_none());
    assert_eq!(platform.opened.lock().unwrap().len(), 1);
    assert_eq!(platform.copied.lock().unwrap().len(), 1);
    runtime.shutdown().unwrap();
}

#[test]
fn wrong_window_or_account_is_rejected_before_credentials_or_http() {
    let transport = Arc::new(FakeTransport::success());
    let (runtime, credentials, handler) = make_runtime(transport.clone());
    let mut stale_window = token(8);
    stale_window.window.foreground = ForegroundGeneration::new(6);
    assert!(handler.execute(effect(stale_window)).is_err());
    let mut stale_account = token(9);
    stale_account.account_generation = CloudAccountGeneration::new(8);
    assert!(handler.execute(effect(stale_account)).is_err());
    assert_eq!(credentials.reads.load(Ordering::SeqCst), 0);
    assert!(transport.requests.lock().unwrap().is_empty());
    runtime.shutdown().unwrap();
}

#[test]
fn detach_resolves_transport_cancellation_and_runtime_reopens_cleanly() {
    let (started_tx, started_rx) = mpsc::channel();
    let (runtime, _, handler) = make_runtime(Arc::new(FakeTransport::waiting(started_tx)));
    let worker = std::thread::spawn(move || handler.execute(effect(token(8))));
    started_rx.recv().unwrap();
    runtime.detach();
    assert!(worker.join().unwrap().unwrap().is_none());

    runtime
        .attach(
            WindowAttachmentGeneration::new(3),
            ForegroundGeneration::new(5),
            Some(owner()),
        )
        .unwrap();
    runtime.shutdown().unwrap();
}

#[test]
fn exceptional_lifetime_drop_cancels_cloud_work_joins_workers_and_stops_runtime() {
    let (started_tx, started_rx) = mpsc::channel();
    let (runtime, _, handler) = make_runtime(Arc::new(FakeTransport::waiting(started_tx)));
    let (results, _receiver) = catalog_result_channel();
    let executor = CatalogEffectExecutor::start(handler, results, Arc::new(NoopWake)).unwrap();
    let lifetime = CatalogCloudLifetime::new(Some(runtime), executor);
    lifetime
        .executor()
        .unwrap()
        .try_submit(effect(token(8)))
        .unwrap();
    started_rx.recv_timeout(Duration::from_secs(1)).unwrap();

    let (dropped_tx, dropped_rx) = mpsc::channel();
    let dropper = std::thread::spawn(move || {
        drop(lifetime);
        let _ = dropped_tx.send(());
    });
    dropped_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("exceptional owner drop must cancel work and join both catalog workers");
    dropper.join().unwrap();
}

#[test]
fn controller_completion_projects_cloud_rows_after_a_window_reopen() {
    let (runtime, _, handler) = make_runtime(Arc::new(FakeTransport::success()));
    let mut controller = CatalogController::new(Arc::new(TestDays)).unwrap();
    controller
        .set_cloud_context(Some(owner()), CatalogCloudPreferences::default())
        .unwrap();
    controller
        .attach(
            WindowAttachmentGeneration::new(3),
            ForegroundGeneration::new(5),
        )
        .unwrap();
    let effects = controller
        .dispatch(CatalogAction::SetSource {
            source: CatalogSource::Cloud,
        })
        .unwrap();
    let refresh = effects
        .into_iter()
        .find(|effect| matches!(effect, CatalogEffect::RefreshCloud { .. }))
        .unwrap();
    let completion = handler.execute(refresh).unwrap().unwrap();
    controller.accept(completion.result).unwrap();
    assert_eq!(controller.state().projection.rows.len(), 1);
    assert_eq!(controller.state().projection.rows[0].title, "Cloud clip");

    runtime.detach();
    controller.detach().unwrap();
    runtime
        .attach(
            WindowAttachmentGeneration::new(4),
            ForegroundGeneration::new(6),
            Some(owner()),
        )
        .unwrap();
    controller
        .attach(
            WindowAttachmentGeneration::new(4),
            ForegroundGeneration::new(6),
        )
        .unwrap();
    assert_eq!(controller.state().projection.rows.len(), 1);
    runtime.shutdown().unwrap();
}

#[test]
fn plain_catalog_worker_can_block_on_cloud_with_the_real_settings_account_port() {
    let directory = TestDir::new("slint-cloud-runtime", "plain-catalog-worker");
    let profile = SettingsProfile::isolated(directory.path());
    let mut settings = AppSettings {
        media_dir: profile.default_media_dir().display().to_string(),
        ..AppSettings::default()
    };
    settings.cloud.host_url = "https://clips.example".into();
    settings.cloud.public_url = Some("https://clips.example".into());
    settings.cloud.connected_user_id = Some("user-1".into());
    settings.cloud.connected_username = Some("user".into());
    settings.cloud.connected_display_name = Some("User".into());
    settings.cloud.credential_target = Some("credential-a".into());
    settings.save_to(profile.settings_path()).unwrap();

    let accounts = Arc::new(SettingsCloudAccountPort::new(SettingsStore::open(profile)));
    let runtime = NativeCloudRuntime::with_transport(
        accounts,
        Arc::new(FakeCredential::default()),
        Arc::new(FakeTransport::success()),
    )
    .unwrap();
    let cloud_owner = runtime.account_context().unwrap().0.unwrap();
    runtime
        .attach(
            WindowAttachmentGeneration::new(3),
            ForegroundGeneration::new(5),
            Some(cloud_owner.clone()),
        )
        .unwrap();
    let token = CloudWorkToken {
        window: WindowWorkToken {
            attachment: WindowAttachmentGeneration::new(3),
            foreground: ForegroundGeneration::new(5),
            request: RequestGeneration::new(19),
        },
        account_key: cloud_owner.account_key,
        account_generation: cloud_owner.account_generation,
    };
    let handler = runtime.effect_handler(Arc::new(RejectLocal));
    let completion = std::thread::spawn(move || handler.execute(effect(token)))
        .join()
        .expect("plain catalog worker must not panic")
        .unwrap()
        .unwrap();
    assert!(matches!(completion.result, CatalogResult::CloudPage(_)));
    runtime.shutdown().unwrap();
}
