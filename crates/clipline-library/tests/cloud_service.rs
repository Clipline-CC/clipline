use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

use clipline_library::ports::{
    AvatarTransportResult, CloudAccountPort, CloudCredential, CloudCredentialPort,
    CloudProfilePatch, CloudRequestFence, CloudTransport, CloudTransportFuture, PortError,
};
use clipline_library::{
    CatalogRevision, CloudAccountGeneration, CloudAccountKey, CloudAccountSnapshot, CloudAvatar,
    CloudClipSummary, CloudListQuery, CloudListTransportRequest, CloudListTransportResponse,
    CloudNextPage, CloudPageNumber, CloudPageOutcome, CloudProfileTransport, CloudService,
    CloudServiceAccount, CloudServiceError, CloudWorkToken, ForegroundGeneration,
    RequestGeneration, WindowAttachmentGeneration, WindowWorkToken, CLOUD_AVATAR_MAX_BYTES,
    CLOUD_LEGACY_PAGE_SIZE, CLOUD_PAGE_SIZE, MAX_CATALOG_STRING_BYTES,
};

fn block_on<F: Future>(future: F) -> F::Output {
    struct ThreadWake(std::thread::Thread);
    impl Wake for ThreadWake {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }

    let waker = Waker::from(Arc::new(ThreadWake(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = Box::pin(future);
    loop {
        match Pin::new(&mut future).poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::park(),
        }
    }
}

fn key(value: &str) -> CloudAccountKey {
    CloudAccountKey::new(value).unwrap()
}

fn work_token(account: &CloudServiceAccount) -> CloudWorkToken {
    CloudWorkToken {
        window: WindowWorkToken {
            attachment: WindowAttachmentGeneration::new(3),
            foreground: ForegroundGeneration::new(4),
            request: RequestGeneration::new(5),
        },
        account_key: account.snapshot.account_key.clone(),
        account_generation: account.snapshot.generation,
    }
}

fn sample_account() -> CloudServiceAccount {
    CloudServiceAccount {
        snapshot: CloudAccountSnapshot {
            account_key: key("https://api.example.test|user-1|credential-1"),
            generation: CloudAccountGeneration::new(7),
            connected: true,
            host_url: "https://api.example.test/base".into(),
            public_url: Some("https://clips.example.test/site".into()),
            username: Some("Dain 98".into()),
            display_name: Some("Dain".into()),
            user_id: Some("user-1".into()),
            default_visibility: "private".into(),
            delete_local_after_upload: false,
            auto_upload_rules: false,
        },
        credential_target: Some("credential-1".into()),
        local_paths_by_clip_id: BTreeMap::from([(
            "local-1".into(),
            r"D:\Videos\Clipline\one.mp4".into(),
        )]),
    }
}

fn summary(index: usize) -> CloudClipSummary {
    CloudClipSummary {
        remote_clip_id: format!("remote-{index}"),
        local_clip_id: (index == 1).then(|| "local-1".into()),
        title: format!("Clip {index}"),
        public_url: Some(format!("https://clips.example.test/c/{index}")),
        visibility: "public".into(),
        status: "ready".into(),
        updated_at_unix: 100 + index as u64,
        uploaded_at_unix: Some(90 + index as u64),
        duration_ms: Some(1_000),
        file_size_bytes: Some(2_000),
        source_type: Some("replay".into()),
    }
}

#[derive(Default)]
struct FakeAccount {
    current: Mutex<Option<CloudServiceAccount>>,
    patches: Mutex<Vec<CloudProfilePatch>>,
}

impl FakeAccount {
    fn with(account: CloudServiceAccount) -> Self {
        Self {
            current: Mutex::new(Some(account)),
            patches: Mutex::new(Vec::new()),
        }
    }

    fn replace(&self, account: CloudServiceAccount) {
        *self.current.lock().unwrap() = Some(account);
    }
}

impl CloudAccountPort for FakeAccount {
    fn snapshot(&self) -> Result<CloudServiceAccount, PortError> {
        self.current
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| PortError::new("no account"))
    }

    fn apply_profile(
        &self,
        expected_key: &CloudAccountKey,
        expected_generation: CloudAccountGeneration,
        patch: CloudProfilePatch,
    ) -> Result<CloudServiceAccount, PortError> {
        let mut current = self.current.lock().unwrap();
        let account = current
            .as_mut()
            .ok_or_else(|| PortError::new("no account"))?;
        if &account.snapshot.account_key != expected_key
            || account.snapshot.generation != expected_generation
        {
            return Err(PortError::account_changed());
        }
        account.snapshot.username = Some(patch.username.clone());
        account
            .snapshot
            .display_name
            .clone_from(&patch.display_name);
        account.snapshot.user_id = Some(patch.user_id.clone());
        self.patches.lock().unwrap().push(patch);
        Ok(account.clone())
    }
}

struct FakeCredential;

impl CloudCredentialPort for FakeCredential {
    fn read(&self, target: &str) -> Result<CloudCredential, PortError> {
        if target == "credential-1" {
            Ok(CloudCredential::new("secret-token"))
        } else {
            Err(PortError::new("missing credential"))
        }
    }
}

#[derive(Default)]
struct FakeFence {
    current: Mutex<Option<CloudWorkToken>>,
}

impl FakeFence {
    fn set(&self, token: CloudWorkToken) {
        *self.current.lock().unwrap() = Some(token);
    }

    fn clear(&self) {
        *self.current.lock().unwrap() = None;
    }
}

impl CloudRequestFence for FakeFence {
    fn is_current(&self, token: &CloudWorkToken) -> bool {
        self.current.lock().unwrap().as_ref() == Some(token)
    }
}

type ListHandler = dyn Fn(&CloudListTransportRequest) -> Result<CloudListTransportResponse, PortError>
    + Send
    + Sync;

struct FakeTransport {
    requests: Mutex<Vec<CloudListTransportRequest>>,
    list: Box<ListHandler>,
    profile: Mutex<Option<Result<CloudProfileTransport, PortError>>>,
    avatars: Mutex<VecDeque<Result<AvatarTransportResult, PortError>>>,
    after_list: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl FakeTransport {
    fn new(
        list: impl Fn(&CloudListTransportRequest) -> Result<CloudListTransportResponse, PortError>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            list: Box::new(list),
            profile: Mutex::new(None),
            avatars: Mutex::new(VecDeque::new()),
            after_list: Mutex::new(None),
        }
    }
}

impl CloudTransport for FakeTransport {
    fn list<'a>(
        &'a self,
        _account: &'a CloudServiceAccount,
        credential: &'a CloudCredential,
        request: &'a CloudListTransportRequest,
        _cancellation: &'a dyn CloudRequestFence,
        _token: &'a CloudWorkToken,
    ) -> CloudTransportFuture<'a, CloudListTransportResponse> {
        assert_eq!(credential.expose(), "secret-token");
        self.requests.lock().unwrap().push(request.clone());
        let result = (self.list)(request);
        if let Some(after) = self.after_list.lock().unwrap().take() {
            after();
        }
        Box::pin(async move { result })
    }

    fn profile<'a>(
        &'a self,
        _account: &'a CloudServiceAccount,
        _credential: &'a CloudCredential,
        _cancellation: &'a dyn CloudRequestFence,
        _token: &'a CloudWorkToken,
    ) -> CloudTransportFuture<'a, CloudProfileTransport> {
        let result = self.profile.lock().unwrap().take().unwrap();
        Box::pin(async move { result })
    }

    fn avatar<'a>(
        &'a self,
        _account: &'a CloudServiceAccount,
        _credential: &'a CloudCredential,
        _etag: Option<&'a str>,
        _cancellation: &'a dyn CloudRequestFence,
        _token: &'a CloudWorkToken,
    ) -> CloudTransportFuture<'a, AvatarTransportResult> {
        let result = self.avatars.lock().unwrap().pop_front().unwrap();
        Box::pin(async move { result })
    }
}

fn service(account: Arc<FakeAccount>, transport: Arc<FakeTransport>) -> CloudService {
    CloudService::new(account, Arc::new(FakeCredential), transport)
}

#[test]
fn page_requests_are_fixed_at_sixty_and_full_pages_expose_conservative_next() {
    let account = Arc::new(FakeAccount::with(sample_account()));
    let transport = Arc::new(FakeTransport::new(|request| {
        Ok(CloudListTransportResponse {
            page: request.page,
            page_size: request.page_size,
            clips: (0..request.page_size as usize).map(summary).collect(),
        })
    }));
    let fence = FakeFence::default();
    let token = work_token(&account.snapshot().unwrap());
    fence.set(token.clone());
    let query = CloudListQuery {
        query: Some("ranked".into()),
        visibility: Some("public".into()),
        sort: "title_asc".into(),
        ..CloudListQuery::default()
    };

    let result = block_on(service(account, transport.clone()).list_page(
        token.clone(),
        &fence,
        CatalogRevision::new(11),
        CloudPageNumber::new(3).unwrap(),
        query.clone(),
    ))
    .unwrap();

    assert_eq!(result.token, token);
    assert_eq!(result.revision, CatalogRevision::new(11));
    let CloudPageOutcome::Page { page, items, next } = result.outcome else {
        panic!("expected page")
    };
    assert_eq!(page, CloudPageNumber::new(3).unwrap());
    assert_eq!(items.len(), CLOUD_PAGE_SIZE);
    assert_eq!(
        next,
        CloudNextPage::Probe {
            page: CloudPageNumber::new(4).unwrap()
        }
    );
    assert_eq!(items[1].path, r"D:\Videos\Clipline\one.mp4");
    assert_eq!(transport.requests.lock().unwrap()[0].page, 3);
    assert_eq!(transport.requests.lock().unwrap()[0].page_size, 60);
    assert_eq!(transport.requests.lock().unwrap()[0].query, query);
}

#[test]
fn short_first_page_is_terminal_and_status_preserves_legacy_fields() {
    let account = Arc::new(FakeAccount::with(sample_account()));
    let transport = Arc::new(FakeTransport::new(|request| {
        Ok(CloudListTransportResponse {
            page: request.page,
            page_size: request.page_size,
            clips: vec![summary(0), summary(1)],
        })
    }));
    let fence = FakeFence::default();
    let token = work_token(&account.snapshot().unwrap());
    fence.set(token.clone());
    let service = service(account, transport);

    let result = block_on(service.list_page(
        token.clone(),
        &fence,
        CatalogRevision::new(2),
        CloudPageNumber::new(1).unwrap(),
        CloudListQuery::default(),
    ))
    .unwrap();
    assert!(matches!(
        result.outcome,
        CloudPageOutcome::Page {
            next: CloudNextPage::Terminal,
            ..
        }
    ));

    let status = service.status(token.clone(), &fence).unwrap();
    assert_eq!(status.token, token);
    assert!(status.value.connected);
    assert!(status.value.token_present);
    assert_eq!(status.value.display_name.as_deref(), Some("Dain"));
    assert_eq!(status.value.default_visibility, "private");
}

#[test]
fn short_page_is_terminal_and_empty_following_page_steps_back() {
    let account = Arc::new(FakeAccount::with(sample_account()));
    let transport = Arc::new(FakeTransport::new(|request| {
        let clips = match request.page {
            3 => Vec::new(),
            _ => panic!("unexpected page {}", request.page),
        };
        Ok(CloudListTransportResponse {
            page: request.page,
            page_size: request.page_size,
            clips,
        })
    }));
    let fence = FakeFence::default();
    let token = work_token(&account.snapshot().unwrap());
    fence.set(token.clone());

    let result = block_on(service(account, transport.clone()).list_page(
        token,
        &fence,
        CatalogRevision::new(12),
        CloudPageNumber::new(3).unwrap(),
        CloudListQuery::default(),
    ))
    .unwrap();

    assert_eq!(
        result.outcome,
        CloudPageOutcome::PastEnd {
            requested_page: CloudPageNumber::new(3).unwrap(),
            fallback_page: CloudPageNumber::new(2).unwrap(),
        }
    );
    assert_eq!(
        transport
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| request.page)
            .collect::<Vec<_>>(),
        vec![3]
    );
}

#[test]
fn account_and_window_fences_are_rechecked_after_transport() {
    let initial = sample_account();
    let account = Arc::new(FakeAccount::with(initial.clone()));
    let transport = Arc::new(FakeTransport::new(|request| {
        Ok(CloudListTransportResponse {
            page: request.page,
            page_size: request.page_size,
            clips: vec![summary(0)],
        })
    }));
    let replacement = {
        let mut value = initial.clone();
        value.snapshot.account_key = key("https://other.example|user-2|credential-2");
        value.snapshot.generation = CloudAccountGeneration::new(8);
        value
    };
    let account_for_hook = account.clone();
    *transport.after_list.lock().unwrap() = Some(Box::new(move || {
        account_for_hook.replace(replacement.clone());
    }));
    let fence = FakeFence::default();
    let token = work_token(&initial);
    fence.set(token.clone());

    let error = block_on(service(account, transport).list_page(
        token,
        &fence,
        CatalogRevision::new(1),
        CloudPageNumber::new(1).unwrap(),
        CloudListQuery::default(),
    ))
    .unwrap_err();

    assert_eq!(error, CloudServiceError::AccountChanged);

    let account = Arc::new(FakeAccount::with(initial.clone()));
    let transport = Arc::new(FakeTransport::new(|request| {
        Ok(CloudListTransportResponse {
            page: request.page,
            page_size: request.page_size,
            clips: vec![summary(0)],
        })
    }));
    let fence = Arc::new(FakeFence::default());
    let token = work_token(&initial);
    fence.set(token.clone());
    let fence_for_hook = fence.clone();
    *transport.after_list.lock().unwrap() = Some(Box::new(move || fence_for_hook.clear()));
    let error = block_on(service(account, transport).list_page(
        token,
        fence.as_ref(),
        CatalogRevision::new(1),
        CloudPageNumber::new(1).unwrap(),
        CloudListQuery::default(),
    ))
    .unwrap_err();
    assert_eq!(error, CloudServiceError::StaleWork);
}

#[test]
fn transport_cancellation_is_reported_as_stale_work() {
    let account = Arc::new(FakeAccount::with(sample_account()));
    let transport = Arc::new(FakeTransport::new(|_| Err(PortError::canceled())));
    let fence = FakeFence::default();
    let token = work_token(&account.snapshot().unwrap());
    fence.set(token.clone());

    let error = block_on(service(account, transport).list_page(
        token,
        &fence,
        CatalogRevision::new(1),
        CloudPageNumber::new(1).unwrap(),
        CloudListQuery::default(),
    ))
    .unwrap_err();

    assert_eq!(error, CloudServiceError::StaleWork);
}

#[test]
fn oversized_rows_and_strings_fail_closed() {
    let account = Arc::new(FakeAccount::with(sample_account()));
    let transport = Arc::new(FakeTransport::new(|request| {
        Ok(CloudListTransportResponse {
            page: request.page,
            page_size: request.page_size,
            clips: (0..=CLOUD_PAGE_SIZE).map(summary).collect(),
        })
    }));
    let fence = FakeFence::default();
    let token = work_token(&account.snapshot().unwrap());
    fence.set(token.clone());
    assert!(matches!(
        block_on(service(account, transport).list_page(
            token,
            &fence,
            CatalogRevision::new(1),
            CloudPageNumber::new(1).unwrap(),
            CloudListQuery::default(),
        )),
        Err(CloudServiceError::InvalidResponse(_))
    ));

    let account = Arc::new(FakeAccount::with(sample_account()));
    let transport = Arc::new(FakeTransport::new(|request| {
        let mut clip = summary(0);
        clip.title = "x".repeat(MAX_CATALOG_STRING_BYTES + 1);
        Ok(CloudListTransportResponse {
            page: request.page,
            page_size: request.page_size,
            clips: vec![clip],
        })
    }));
    let fence = FakeFence::default();
    let token = work_token(&account.snapshot().unwrap());
    fence.set(token.clone());
    assert!(matches!(
        block_on(service(account, transport).list_page(
            token,
            &fence,
            CatalogRevision::new(1),
            CloudPageNumber::new(1).unwrap(),
            CloudListQuery::default(),
        )),
        Err(CloudServiceError::InvalidResponse(_))
    ));
}

#[test]
fn legacy_collector_uses_one_hundred_row_pages_and_deduplicates() {
    let account = Arc::new(FakeAccount::with(sample_account()));
    let transport = Arc::new(FakeTransport::new(|request| {
        let clips = if request.page == 1 {
            (0..100).map(summary).collect()
        } else {
            vec![summary(99), summary(100)]
        };
        Ok(CloudListTransportResponse {
            page: request.page,
            page_size: request.page_size,
            clips,
        })
    }));
    let fence = FakeFence::default();
    let token = work_token(&account.snapshot().unwrap());
    fence.set(token.clone());

    let result = block_on(service(account, transport.clone()).legacy_list(token, &fence)).unwrap();

    assert_eq!(result.value.clips.len(), 101);
    assert!(!result.value.truncated);
    assert_eq!(
        transport
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| request.page_size)
            .collect::<Vec<_>>(),
        vec![CLOUD_LEGACY_PAGE_SIZE as u16; 2]
    );
}

#[test]
fn legacy_collector_stops_at_the_exact_ten_thousand_item_ceiling() {
    let account = Arc::new(FakeAccount::with(sample_account()));
    let transport = Arc::new(FakeTransport::new(|request| {
        let first = (request.page as usize - 1) * CLOUD_LEGACY_PAGE_SIZE;
        Ok(CloudListTransportResponse {
            page: request.page,
            page_size: request.page_size,
            clips: (first..first + CLOUD_LEGACY_PAGE_SIZE)
                .map(summary)
                .collect(),
        })
    }));
    let fence = FakeFence::default();
    let token = work_token(&account.snapshot().unwrap());
    fence.set(token.clone());

    let result = block_on(service(account, transport.clone()).legacy_list(token, &fence)).unwrap();

    assert_eq!(result.value.clips.len(), 10_000);
    assert!(result.value.truncated);
    assert_eq!(transport.requests.lock().unwrap().len(), 100);
}

#[test]
fn profile_refresh_is_account_fenced_and_browser_effects_are_typed() {
    let account = Arc::new(FakeAccount::with(sample_account()));
    let transport = Arc::new(FakeTransport::new(|_| unreachable!()));
    *transport.profile.lock().unwrap() = Some(Ok(CloudProfileTransport {
        user_id: "user-1".into(),
        username: "Dain 98".into(),
        display_name: Some("  Dain  ".into()),
    }));
    let fence = FakeFence::default();
    let token = work_token(&account.snapshot().unwrap());
    fence.set(token.clone());

    let service = service(account.clone(), transport);
    let result = block_on(service.profile(token.clone(), &fence)).unwrap();
    assert_eq!(result.value.display_name.as_deref(), Some("Dain"));
    assert_eq!(
        result.value.profile_url,
        "https://clips.example.test/site/u/Dain%2098"
    );
    assert_eq!(account.patches.lock().unwrap().len(), 1);

    let profile = service.open_profile_effect(token.clone(), &fence).unwrap();
    assert_eq!(profile.token, token);
    assert_eq!(profile.value.context, "cloud user profile");
    assert_eq!(
        profile.value.url,
        "https://clips.example.test/site/u/Dain%2098"
    );
    let clip = service
        .open_clip_effect(profile.token, &fence, "remote-1_ABC")
        .unwrap();
    assert_eq!(
        clip.value.url,
        "https://clips.example.test/site/clip/remote-1_ABC"
    );
    assert!(service
        .open_clip_effect(clip.token, &fence, "../escape")
        .is_err());
}

#[test]
fn avatar_cache_honors_etag_not_modified_missing_and_byte_limit() {
    let account = Arc::new(FakeAccount::with(sample_account()));
    let transport = Arc::new(FakeTransport::new(|_| unreachable!()));
    transport.avatars.lock().unwrap().extend([
        Ok(AvatarTransportResult::Fresh {
            content_type: Some("image/png; charset=binary".into()),
            etag: Some("etag-1".into()),
            bytes: vec![1, 2, 3],
        }),
        Ok(AvatarTransportResult::NotModified),
        Ok(AvatarTransportResult::Missing),
        Ok(AvatarTransportResult::Fresh {
            content_type: Some("image/png".into()),
            etag: None,
            bytes: vec![0; CLOUD_AVATAR_MAX_BYTES + 1],
        }),
    ]);
    let fence = FakeFence::default();
    let token = work_token(&account.snapshot().unwrap());
    fence.set(token.clone());
    let service = service(account, transport);

    let fresh = block_on(service.avatar(token.clone(), &fence)).unwrap();
    assert_eq!(
        fresh.value,
        Some(CloudAvatar {
            content_type: "image/png".into(),
            etag: Some("etag-1".into()),
            bytes: vec![1, 2, 3],
        })
    );
    let cached = block_on(service.avatar(token.clone(), &fence)).unwrap();
    assert_eq!(cached.value, fresh.value);
    let missing = block_on(service.avatar(token.clone(), &fence)).unwrap();
    assert_eq!(missing.value, None);
    assert!(matches!(
        block_on(service.avatar(token, &fence)),
        Err(CloudServiceError::InvalidResponse(_))
    ));
}
