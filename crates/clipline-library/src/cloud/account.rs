//! Exact, framework-neutral Clipline Cloud account mutation service.

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Mutex, OnceLock};

use clipline_settings::{
    normalize_cloud_visibility, CloudAccountProfile, MAX_CLOUD_ACCOUNT_FIELD_BYTES,
};
use clipline_shell::secret::SecretString;

use super::ports::{CloudCredential, PortError};
use super::protocol::{CloudApiBase, CloudProtocolError};
use super::{CloudConnectionStatus, CloudServiceAccount};

const DEFAULT_DEVICE_NAME: &str = "Clipline Desktop";
pub const MAX_CLOUD_PASSWORD_BYTES: usize = 64 * 1024;

static CLOUD_ACCOUNT_OPERATION: OnceLock<Mutex<()>> = OnceLock::new();

fn account_operation() -> &'static Mutex<()> {
    CLOUD_ACCOUNT_OPERATION.get_or_init(|| Mutex::new(()))
}

/// Move-only, zeroizing Cloud password ownership.
pub struct CloudPassword(SecretString);

impl CloudPassword {
    pub fn new(value: String) -> Result<Self, CloudAccountServiceError> {
        let value = SecretString::new(value);
        if value.expose_secret().is_empty()
            || value.expose_secret().len() > MAX_CLOUD_PASSWORD_BYTES
        {
            return Err(CloudAccountServiceError::InvalidPassword);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        self.0.expose_secret()
    }
}

impl fmt::Debug for CloudPassword {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CloudPassword([REDACTED])")
    }
}

pub struct CloudConnectRequest {
    pub host_url: String,
    pub username: String,
    pub password: CloudPassword,
    pub device_name: Option<String>,
    pub plain_http_confirmed: bool,
    pub default_visibility: Option<String>,
}

impl fmt::Debug for CloudConnectRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CloudConnectRequest")
            .field("host_url", &self.host_url)
            .field("username", &self.username)
            .field("password", &"[REDACTED]")
            .field("device_name", &self.device_name)
            .field("plain_http_confirmed", &self.plain_http_confirmed)
            .field("default_visibility", &self.default_visibility)
            .finish()
    }
}

pub struct CloudAuthenticationRequest {
    pub base: CloudApiBase,
    pub username: String,
    pub password: CloudPassword,
    pub device_name: String,
    pub default_visibility: Option<String>,
}

pub struct CloudAuthenticatedAccount {
    pub host_url: String,
    pub public_url: String,
    pub user_id: String,
    pub username: String,
    pub display_name: Option<String>,
    pub credential: CloudCredential,
}

pub type CloudAccountFuture<'a> = Pin<
    Box<dyn Future<Output = Result<CloudAuthenticatedAccount, CloudProtocolError>> + Send + 'a>,
>;

pub type CloudAccountCancellationFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

pub trait CloudAccountCancellation: Send + Sync {
    fn is_canceled(&self) -> bool;

    fn cancelled<'a>(&'a self) -> CloudAccountCancellationFuture<'a> {
        Box::pin(std::future::pending())
    }
}

#[derive(Debug, Default)]
pub struct NeverCancelCloudAccount;

impl CloudAccountCancellation for NeverCancelCloudAccount {
    fn is_canceled(&self) -> bool {
        false
    }
}

pub trait CloudAccountTransport: Send + Sync {
    fn authenticate<'a>(
        &'a self,
        request: CloudAuthenticationRequest,
        cancellation: &'a dyn CloudAccountCancellation,
    ) -> CloudAccountFuture<'a>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudAccountState {
    pub account: CloudServiceAccount,
    pub profile: CloudAccountProfile,
}

pub trait CloudAccountMutationPort: Send + Sync {
    fn load(&self) -> Result<CloudAccountState, PortError>;
    fn reserve_credential(
        &self,
        expected: &CloudAccountState,
        target: String,
    ) -> Result<CloudAccountState, PortError>;
    fn commit_connect(
        &self,
        expected: &CloudAccountState,
        connected: CloudConnectedProfile,
        default_visibility: Option<String>,
    ) -> Result<CloudAccountState, PortError>;
    fn commit_disconnect(
        &self,
        expected: &CloudAccountState,
    ) -> Result<CloudAccountState, PortError>;
    fn reconcile_cleanup(
        &self,
        expected: &CloudAccountState,
        deleted_targets: &[String],
    ) -> Result<CloudAccountState, PortError>;
}

pub trait CloudCredentialMutationPort: Send + Sync {
    fn read(&self, target: &str) -> Result<Option<CloudCredential>, PortError>;
    fn write(
        &self,
        target: &str,
        username: &str,
        credential: &CloudCredential,
    ) -> Result<(), PortError>;
    fn delete_if_present(&self, target: &str) -> Result<(), PortError>;
}

pub trait CloudAccountInvalidationPort: Send + Sync {
    fn account_changed(
        &self,
        previous: Option<&CloudServiceAccount>,
        current: Option<&CloudServiceAccount>,
    );
}

#[derive(Debug, Default)]
pub struct IgnoreCloudAccountInvalidation;

impl CloudAccountInvalidationPort for IgnoreCloudAccountInvalidation {
    fn account_changed(
        &self,
        _previous: Option<&CloudServiceAccount>,
        _current: Option<&CloudServiceAccount>,
    ) {
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudConnectedProfile {
    pub host_url: String,
    pub public_url: String,
    pub user_id: String,
    pub username: String,
    pub display_name: Option<String>,
    pub credential_target: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudAccountServiceError {
    InvalidUsername,
    InvalidPassword,
    InvalidDeviceName,
    Canceled,
    AccountChanged,
    CredentialCollision,
    CredentialRead,
    CredentialWrite,
    CredentialDelete,
    CleanupCapacity,
    SettingsUnavailable,
    RollbackIncomplete,
    Protocol,
}

impl fmt::Display for CloudAccountServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidUsername => "Cloud username is invalid",
            Self::InvalidPassword => "Cloud password is invalid",
            Self::InvalidDeviceName => "Cloud device name is invalid",
            Self::Canceled => "Cloud account operation was canceled",
            Self::AccountChanged => "Cloud account changed during the operation",
            Self::CredentialCollision => "Cloud credential target is already occupied",
            Self::CredentialRead => "read Cloud credential failed",
            Self::CredentialWrite => "store Cloud credential failed",
            Self::CredentialDelete => "delete Cloud credential failed",
            Self::CleanupCapacity => "Cloud credential cleanup queue is full",
            Self::SettingsUnavailable => "Cloud settings are unavailable",
            Self::RollbackIncomplete => "Cloud credential rollback requires cleanup",
            Self::Protocol => "Cloud account request failed",
        };
        formatter.write_str(message)
    }
}

impl Error for CloudAccountServiceError {}

pub struct CloudAccountService<S, C, H, I> {
    settings: S,
    credentials: C,
    transport: H,
    invalidation: I,
}

impl<S, C, H, I> CloudAccountService<S, C, H, I>
where
    S: CloudAccountMutationPort,
    C: CloudCredentialMutationPort,
    H: CloudAccountTransport,
    I: CloudAccountInvalidationPort,
{
    #[must_use]
    pub fn new(settings: S, credentials: C, transport: H, invalidation: I) -> Self {
        Self {
            settings,
            credentials,
            transport,
            invalidation,
        }
    }

    pub fn status(&self) -> Result<CloudConnectionStatus, CloudAccountServiceError> {
        let _operation = account_operation()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let loaded = self.load()?;
        let current = self.reconcile_cleanup_best_effort(loaded);
        self.status_for(&current)
    }

    pub async fn connect(
        &self,
        request: CloudConnectRequest,
        cancellation: &dyn CloudAccountCancellation,
    ) -> Result<CloudConnectionStatus, CloudAccountServiceError> {
        let expected = self.load()?;
        let request = normalize_connect_request(request)?;
        let default_visibility = request.default_visibility.clone();
        if cancellation.is_canceled() {
            return Err(CloudAccountServiceError::Canceled);
        }
        let authenticated = self
            .transport
            .authenticate(request, cancellation)
            .await
            .map_err(map_protocol_error)?;
        validate_authenticated(&authenticated)?;
        if cancellation.is_canceled() {
            return Err(CloudAccountServiceError::Canceled);
        }

        let _operation = account_operation()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cancellation.is_canceled() {
            return Err(CloudAccountServiceError::Canceled);
        }
        let current = self.load()?;
        if current.account.snapshot.account_key != expected.account.snapshot.account_key
            || current.account.snapshot.generation != expected.account.snapshot.generation
        {
            return Err(CloudAccountServiceError::AccountChanged);
        }
        let target = clipline_settings::cloud::cloud_credential_target_for_operation(
            &uuid::Uuid::new_v4().simple().to_string(),
        )
        .map_err(|_| CloudAccountServiceError::SettingsUnavailable)?;
        if self
            .credentials
            .read(&target)
            .map_err(|_| CloudAccountServiceError::CredentialRead)?
            .is_some()
        {
            return Err(CloudAccountServiceError::CredentialCollision);
        }
        let reserved = self
            .settings
            .reserve_credential(&current, target.clone())
            .map_err(map_port_error)?;
        if self
            .credentials
            .write(&target, &authenticated.username, &authenticated.credential)
            .is_err()
        {
            let _ = self.cleanup_candidate(reserved, &target);
            return Err(CloudAccountServiceError::CredentialWrite);
        }
        let connected = CloudConnectedProfile {
            host_url: authenticated.host_url,
            public_url: authenticated.public_url,
            user_id: authenticated.user_id,
            username: authenticated.username,
            display_name: authenticated.display_name,
            credential_target: target.clone(),
        };
        let committed = match self
            .settings
            .commit_connect(&reserved, connected, default_visibility)
        {
            Ok(committed) => committed,
            Err(error) => {
                // Publication errors are ambiguous: the replacement may be
                // durable even when its acknowledgement failed. Re-read the
                // exact owner before deleting the newly written credential.
                // A committed profile owns the credential; a still-reserved
                // cleanup target proves that deletion is safe. Indeterminate
                // states retain both the credential and durable cleanup work.
                let observed = self.load()?;
                if observed.profile.credential_target.as_deref() == Some(target.as_str()) {
                    observed
                } else if observed
                    .profile
                    .credential_cleanup_targets
                    .contains(&target)
                {
                    if self.cleanup_candidate(observed, &target).is_err() {
                        return Err(CloudAccountServiceError::RollbackIncomplete);
                    }
                    return Err(map_port_error(error));
                } else {
                    return Err(map_port_error(error));
                }
            }
        };
        self.invalidation
            .account_changed(Some(&current.account), Some(&committed.account));
        let committed = self.reconcile_cleanup_best_effort(committed);
        self.status_for(&committed)
    }

    pub fn disconnect(&self) -> Result<CloudConnectionStatus, CloudAccountServiceError> {
        let _operation = account_operation()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current = self.load()?;
        let disconnected = match self.settings.commit_disconnect(&current) {
            Ok(disconnected) => disconnected,
            Err(error) => {
                let observed = self.load()?;
                if observed.profile.connected() {
                    return Err(map_port_error(error));
                }
                observed
            }
        };
        self.invalidation
            .account_changed(Some(&current.account), None);
        let disconnected = self.reconcile_cleanup_best_effort(disconnected);
        self.status_for(&disconnected)
    }

    fn load(&self) -> Result<CloudAccountState, CloudAccountServiceError> {
        self.settings
            .load()
            .map_err(|_| CloudAccountServiceError::SettingsUnavailable)
    }

    fn status_for(
        &self,
        state: &CloudAccountState,
    ) -> Result<CloudConnectionStatus, CloudAccountServiceError> {
        let token_present = match state.profile.credential_target.as_deref() {
            Some(target) => self
                .credentials
                .read(target)
                .map(|credential| credential.is_some())
                .unwrap_or(false),
            None => false,
        };
        let snapshot = &state.account.snapshot;
        Ok(CloudConnectionStatus {
            connected: snapshot.connected && token_present,
            token_present,
            host_url: snapshot.host_url.clone(),
            public_url: snapshot.public_url.clone(),
            username: snapshot.username.clone(),
            display_name: snapshot.display_name.clone(),
            user_id: snapshot.user_id.clone(),
            default_visibility: snapshot.default_visibility.clone(),
            delete_local_after_upload: snapshot.delete_local_after_upload,
            auto_upload_rules: snapshot.auto_upload_rules,
        })
    }

    fn reconcile_cleanup_best_effort(&self, state: CloudAccountState) -> CloudAccountState {
        let mut deleted = Vec::new();
        for target in &state.profile.credential_cleanup_targets {
            if self.credentials.delete_if_present(target).is_ok() {
                deleted.push(target.clone());
            }
        }
        if deleted.is_empty() {
            return state;
        }
        self.settings
            .reconcile_cleanup(&state, &deleted)
            .unwrap_or(state)
    }

    fn cleanup_candidate(
        &self,
        state: CloudAccountState,
        target: &str,
    ) -> Result<(), CloudAccountServiceError> {
        self.credentials
            .delete_if_present(target)
            .map_err(|_| CloudAccountServiceError::CredentialDelete)?;
        self.settings
            .reconcile_cleanup(&state, &[target.to_owned()])
            .map(|_| ())
            .map_err(map_port_error)
    }
}

fn normalize_connect_request(
    request: CloudConnectRequest,
) -> Result<CloudAuthenticationRequest, CloudAccountServiceError> {
    let username = bounded_required(
        "username",
        request.username,
        CloudAccountServiceError::InvalidUsername,
    )?;
    let device_name = request.device_name.unwrap_or_default();
    let device_name = if device_name.trim().is_empty() {
        DEFAULT_DEVICE_NAME.to_owned()
    } else {
        bounded_required(
            "device name",
            device_name,
            CloudAccountServiceError::InvalidDeviceName,
        )?
    };
    let base = CloudApiBase::parse(&request.host_url, request.plain_http_confirmed)
        .map_err(map_protocol_error)?;
    let default_visibility = Some(normalize_cloud_visibility(
        request.default_visibility.as_deref().unwrap_or("private"),
    ));
    Ok(CloudAuthenticationRequest {
        base,
        username,
        password: request.password,
        device_name,
        default_visibility,
    })
}

fn validate_authenticated(
    authenticated: &CloudAuthenticatedAccount,
) -> Result<(), CloudAccountServiceError> {
    for value in [
        authenticated.host_url.as_str(),
        authenticated.public_url.as_str(),
        authenticated.user_id.as_str(),
        authenticated.username.as_str(),
    ] {
        if value.trim().is_empty() || value.len() > MAX_CLOUD_ACCOUNT_FIELD_BYTES {
            return Err(CloudAccountServiceError::Protocol);
        }
    }
    if authenticated
        .display_name
        .as_ref()
        .is_some_and(|value| value.is_empty() || value.len() > MAX_CLOUD_ACCOUNT_FIELD_BYTES)
    {
        return Err(CloudAccountServiceError::Protocol);
    }
    Ok(())
}

fn bounded_required(
    _field: &str,
    value: String,
    error: CloudAccountServiceError,
) -> Result<String, CloudAccountServiceError> {
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_CLOUD_ACCOUNT_FIELD_BYTES {
        return Err(error);
    }
    Ok(value.to_owned())
}

fn map_protocol_error(error: CloudProtocolError) -> CloudAccountServiceError {
    if matches!(error, CloudProtocolError::Canceled) {
        CloudAccountServiceError::Canceled
    } else {
        CloudAccountServiceError::Protocol
    }
}

fn map_port_error(error: PortError) -> CloudAccountServiceError {
    if error.is_account_changed() {
        CloudAccountServiceError::AccountChanged
    } else if error.is_canceled() {
        CloudAccountServiceError::Canceled
    } else if error.to_string().contains("cleanup") && error.to_string().contains("full") {
        CloudAccountServiceError::CleanupCapacity
    } else {
        CloudAccountServiceError::SettingsUnavailable
    }
}
