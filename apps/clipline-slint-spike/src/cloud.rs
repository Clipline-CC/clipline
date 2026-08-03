//! Process-owned Clipline Cloud runtime for the native shell.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(windows)]
use clipline_library::cloud::settings::SettingsCloudAccountPort;
#[cfg(windows)]
use clipline_library::http::ReqwestCloudTransport;
use clipline_library::ports::{
    CloudAccountPort, CloudCancellationFuture, CloudCredentialPort, CloudRequestFence,
    CloudTransport,
};
#[cfg(windows)]
use clipline_library::ports::{CloudCredential, PortError};
use clipline_library::{
    CatalogCloudPreferences, CatalogEffect, CatalogOperationOwner, CatalogResult,
    CatalogUploadVisibility, CloudCatalogOwner, CloudListQuery, CloudService, CloudServiceError,
    CloudWorkToken, ExpectedResultOwner, ForegroundGeneration, WindowAttachmentGeneration,
    MAX_FOREGROUND_MESSAGE_BYTES,
};
#[cfg(windows)]
use clipline_settings::SettingsStore;

use crate::catalog::{CatalogEffectExecutor, CatalogEffectHandler, OwnedCatalogResult};

const CLOUD_RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct WindowOwner {
    attachment: WindowAttachmentGeneration,
    foreground: ForegroundGeneration,
    cloud: Option<CloudCatalogOwner>,
}

struct CurrentRequest {
    token: CloudWorkToken,
    cancellation: Arc<RequestCancellation>,
}

#[derive(Default)]
struct FenceState {
    window: Option<WindowOwner>,
    request: Option<CurrentRequest>,
}

#[derive(Default)]
struct FenceRegistry {
    state: Mutex<FenceState>,
}

impl FenceRegistry {
    fn attach(
        &self,
        attachment: WindowAttachmentGeneration,
        foreground: ForegroundGeneration,
        cloud: Option<CloudCatalogOwner>,
    ) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "native Cloud fence lock is unavailable".to_owned())?;
        cancel_request(&mut state);
        state.window = Some(WindowOwner {
            attachment,
            foreground,
            cloud,
        });
        Ok(())
    }

    fn detach(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        cancel_request(&mut state);
        state.window = None;
    }

    fn begin(self: &Arc<Self>, token: &CloudWorkToken) -> Result<ExactCloudFence, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "native Cloud fence lock is unavailable".to_owned())?;
        let current = state.window.as_ref().is_some_and(|window| {
            window.attachment == token.window.attachment
                && window.foreground == token.window.foreground
                && window.cloud.as_ref().is_some_and(|owner| {
                    owner.account_key == token.account_key
                        && owner.account_generation == token.account_generation
                })
        });
        if !current {
            return Err("native Cloud work belongs to a stale window or account".to_owned());
        }
        cancel_request(&mut state);
        let cancellation = Arc::new(RequestCancellation::default());
        state.request = Some(CurrentRequest {
            token: token.clone(),
            cancellation: Arc::clone(&cancellation),
        });
        Ok(ExactCloudFence {
            registry: Arc::clone(self),
            token: token.clone(),
            cancellation,
        })
    }

    fn is_current(&self, token: &CloudWorkToken, cancellation: &Arc<RequestCancellation>) -> bool {
        if cancellation.is_canceled() {
            return false;
        }
        self.state.lock().ok().is_some_and(|state| {
            state.request.as_ref().is_some_and(|request| {
                request.token == *token && Arc::ptr_eq(&request.cancellation, cancellation)
            })
        })
    }
}

fn cancel_request(state: &mut FenceState) {
    if let Some(request) = state.request.take() {
        request.cancellation.cancel();
    }
}

#[derive(Default)]
struct RequestCancellation {
    canceled: AtomicBool,
    notify: tokio::sync::Notify,
}

impl RequestCancellation {
    fn cancel(&self) {
        if !self.canceled.swap(true, Ordering::AcqRel) {
            self.notify.notify_waiters();
        }
    }

    fn is_canceled(&self) -> bool {
        self.canceled.load(Ordering::Acquire)
    }

    async fn cancelled(&self) {
        loop {
            let notified = self.notify.notified();
            if self.is_canceled() {
                return;
            }
            notified.await;
        }
    }
}

struct ExactCloudFence {
    registry: Arc<FenceRegistry>,
    token: CloudWorkToken,
    cancellation: Arc<RequestCancellation>,
}

impl CloudRequestFence for ExactCloudFence {
    fn is_current(&self, token: &CloudWorkToken) -> bool {
        self.token == *token && self.registry.is_current(token, &self.cancellation)
    }

    fn cancelled<'a>(&'a self, token: &'a CloudWorkToken) -> CloudCancellationFuture<'a> {
        Box::pin(async move {
            if self.token != *token {
                return;
            }
            self.cancellation.cancelled().await;
        })
    }
}

struct CloudShared {
    accounts: Arc<dyn CloudAccountPort>,
    service: CloudService,
    fences: Arc<FenceRegistry>,
    handle: tokio::runtime::Handle,
}

/// Long-lived Cloud owner. Window destruction only cancels its current fence;
/// HTTP clients, account state, and the Tokio runtime survive in tray mode.
pub struct NativeCloudRuntime {
    shared: Arc<CloudShared>,
    runtime: Option<tokio::runtime::Runtime>,
}

impl NativeCloudRuntime {
    #[cfg(windows)]
    pub fn open(store: SettingsStore) -> Result<Self, String> {
        let accounts = Arc::new(SettingsCloudAccountPort::new(store));
        let credentials = Arc::new(WindowsCloudCredentialPort);
        let transport = Arc::new(ReqwestCloudTransport::new().map_err(|error| error.to_string())?);
        Self::with_transport(accounts, credentials, transport)
    }

    pub fn with_transport(
        accounts: Arc<dyn CloudAccountPort>,
        credentials: Arc<dyn CloudCredentialPort>,
        transport: Arc<dyn CloudTransport>,
    ) -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("clipline-cloud")
            .build()
            .map_err(|error| format!("start native Cloud runtime: {error}"))?;
        let service = CloudService::new(Arc::clone(&accounts), credentials, transport);
        let shared = Arc::new(CloudShared {
            accounts,
            service,
            fences: Arc::new(FenceRegistry::default()),
            handle: runtime.handle().clone(),
        });
        Ok(Self {
            shared,
            runtime: Some(runtime),
        })
    }

    pub fn account_context(
        &self,
    ) -> Result<(Option<CloudCatalogOwner>, CatalogCloudPreferences), String> {
        let account = self
            .shared
            .accounts
            .snapshot()
            .map_err(|error| error.to_string())?;
        let preferences = CatalogCloudPreferences {
            default_visibility: match account.snapshot.default_visibility.as_str() {
                "public" => CatalogUploadVisibility::Public,
                "unlisted" => CatalogUploadVisibility::Unlisted,
                _ => CatalogUploadVisibility::Private,
            },
            delete_local_after_upload: account.snapshot.delete_local_after_upload,
        };
        let owner = account.snapshot.connected.then_some(CloudCatalogOwner {
            account_key: account.snapshot.account_key,
            account_generation: account.snapshot.generation,
        });
        Ok((owner, preferences))
    }

    pub fn attach(
        &self,
        attachment: WindowAttachmentGeneration,
        foreground: ForegroundGeneration,
        owner: Option<CloudCatalogOwner>,
    ) -> Result<(), String> {
        self.shared.fences.attach(attachment, foreground, owner)
    }

    pub fn detach(&self) {
        self.shared.fences.detach();
    }

    pub fn effect_handler(
        &self,
        local: Arc<dyn CatalogEffectHandler>,
    ) -> Arc<dyn CatalogEffectHandler> {
        Arc::new(NativeCloudEffectHandler {
            cloud: Arc::clone(&self.shared),
            local,
        })
    }

    pub fn shutdown(mut self) -> Result<(), String> {
        self.detach();
        let runtime = self
            .runtime
            .take()
            .ok_or_else(|| "native Cloud runtime is already shut down".to_owned())?;
        runtime.shutdown_timeout(CLOUD_RUNTIME_SHUTDOWN_TIMEOUT);
        Ok(())
    }
}

/// Ordered process-lifetime owner for the Cloud runtime and the catalog
/// workers that may be blocked inside it. Both normal quit and exceptional
/// shell drop use the same idempotent shutdown path.
pub struct CatalogCloudLifetime {
    cloud: Option<NativeCloudRuntime>,
    executor: Option<CatalogEffectExecutor>,
}

impl CatalogCloudLifetime {
    #[must_use]
    pub fn new(cloud: Option<NativeCloudRuntime>, executor: CatalogEffectExecutor) -> Self {
        Self {
            cloud,
            executor: Some(executor),
        }
    }

    #[must_use]
    pub fn cloud(&self) -> Option<&NativeCloudRuntime> {
        self.cloud.as_ref()
    }

    #[must_use]
    pub fn executor(&self) -> Option<&CatalogEffectExecutor> {
        self.executor.as_ref()
    }

    pub fn shutdown(&mut self) -> Result<(), String> {
        if let Some(cloud) = self.cloud.as_ref() {
            cloud.detach();
        }
        let mut first_error = None;
        if let Some(executor) = self.executor.take() {
            if let Err(error) = executor.shutdown() {
                first_error = Some(error);
            }
        }
        if let Some(cloud) = self.cloud.take() {
            if let Err(error) = cloud.shutdown() {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

impl Drop for CatalogCloudLifetime {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

struct NativeCloudEffectHandler {
    cloud: Arc<CloudShared>,
    local: Arc<dyn CatalogEffectHandler>,
}

impl CatalogEffectHandler for NativeCloudEffectHandler {
    fn execute(&self, effect: CatalogEffect) -> Result<Option<OwnedCatalogResult>, String> {
        effect
            .validate_bounds()
            .map_err(|error| error.to_string())?;
        let CatalogEffect::RefreshCloud {
            token,
            revision,
            page,
            query,
        } = effect
        else {
            return self.local.execute(effect);
        };
        let account = self
            .cloud
            .accounts
            .snapshot()
            .map_err(|error| error.to_string())?;
        if account.snapshot.account_key != token.account_key
            || account.snapshot.generation != token.account_generation
        {
            return Err("native Cloud work belongs to a replaced account".into());
        }
        let fence = self.cloud.fences.begin(&token)?;
        let query = query.trim();
        let request = CloudListQuery {
            query: (!query.is_empty()).then(|| query.to_owned()),
            ..CloudListQuery::default()
        };
        let result = self.cloud.handle.block_on(self.cloud.service.list_page(
            token.clone(),
            &fence,
            revision,
            page,
            request,
        ));
        match result {
            Ok(completion) => Ok(Some(OwnedCatalogResult {
                result: CatalogResult::CloudPage(completion),
                expected: ExpectedResultOwner::Cloud(token),
            })),
            Err(CloudServiceError::StaleWork) => Ok(None),
            Err(error) => {
                let owner = CatalogOperationOwner::CloudRefresh {
                    token,
                    revision,
                    page,
                };
                Ok(Some(OwnedCatalogResult {
                    result: CatalogResult::OperationFailed {
                        owner: owner.clone(),
                        message: bounded_message(error.to_string()),
                    },
                    expected: ExpectedResultOwner::Operation(owner),
                }))
            }
        }
    }
}

fn bounded_message(mut message: String) -> String {
    if message.len() <= MAX_FOREGROUND_MESSAGE_BYTES {
        return message;
    }
    let mut end = MAX_FOREGROUND_MESSAGE_BYTES;
    while end != 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    message
}

#[cfg(windows)]
struct WindowsCloudCredentialPort;

#[cfg(windows)]
impl CloudCredentialPort for WindowsCloudCredentialPort {
    fn read(&self, target: &str) -> Result<CloudCredential, PortError> {
        clipline_shell::windows::credential::CredentialStore::new("cloud token")
            .read(target)
            .map(CloudCredential::new)
            .map_err(|error| PortError::new(error.to_string()))
    }
}
