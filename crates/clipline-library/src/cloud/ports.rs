use std::future::Future;
use std::pin::Pin;

use crate::{CloudAccountGeneration, CloudAccountKey, CloudWorkToken};

use super::{
    CloudListTransportRequest, CloudListTransportResponse, CloudProfileTransport,
    CloudServiceAccount,
};

pub type CloudTransportFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, PortError>> + Send + 'a>>;
pub type CloudCancellationFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PortErrorKind {
    Failed,
    AccountChanged,
    Canceled,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct PortError {
    kind: PortErrorKind,
    message: String,
}

impl PortError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            kind: PortErrorKind::Failed,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn account_changed() -> Self {
        Self {
            kind: PortErrorKind::AccountChanged,
            message: "cloud account changed while work was in flight".into(),
        }
    }

    #[must_use]
    pub fn canceled() -> Self {
        Self {
            kind: PortErrorKind::Canceled,
            message: "cloud request was canceled".into(),
        }
    }

    #[must_use]
    pub const fn is_account_changed(&self) -> bool {
        matches!(self.kind, PortErrorKind::AccountChanged)
    }

    #[must_use]
    pub const fn is_canceled(&self) -> bool {
        matches!(self.kind, PortErrorKind::Canceled)
    }
}

/// Bearer credential kept outside serializable/debuggable service values.
pub struct CloudCredential(String);

impl CloudCredential {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudProfilePatch {
    pub user_id: String,
    pub username: String,
    pub display_name: Option<String>,
}

pub trait CloudAccountPort: Send + Sync {
    fn snapshot(&self) -> Result<CloudServiceAccount, PortError>;

    fn apply_profile(
        &self,
        expected_key: &CloudAccountKey,
        expected_generation: CloudAccountGeneration,
        patch: CloudProfilePatch,
    ) -> Result<CloudServiceAccount, PortError>;
}

pub trait CloudCredentialPort: Send + Sync {
    fn read(&self, target: &str) -> Result<CloudCredential, PortError>;
}

/// Exact window + account cancellation fence owned by the catalog controller.
pub trait CloudRequestFence: Send + Sync {
    fn is_current(&self, token: &CloudWorkToken) -> bool;

    /// Resolves when the exact work token is canceled. Async transports race
    /// their request against this future so cancellation does not wait for an
    /// HTTP read deadline. Controllers without an async notifier may retain
    /// the pending default and rely on pre/post fencing.
    fn cancelled<'a>(&'a self, _token: &'a CloudWorkToken) -> CloudCancellationFuture<'a> {
        Box::pin(std::future::pending())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AvatarTransportResult {
    Missing,
    NotModified,
    Fresh {
        content_type: Option<String>,
        etag: Option<String>,
        bytes: Vec<u8>,
    },
}

/// HTTP is injected so the neutral crate does not own credentials, a runtime,
/// or the AGPL Clipline Cloud client crate.
pub trait CloudTransport: Send + Sync {
    fn list<'a>(
        &'a self,
        account: &'a CloudServiceAccount,
        credential: &'a CloudCredential,
        request: &'a CloudListTransportRequest,
        cancellation: &'a dyn CloudRequestFence,
        token: &'a CloudWorkToken,
    ) -> CloudTransportFuture<'a, CloudListTransportResponse>;

    fn profile<'a>(
        &'a self,
        account: &'a CloudServiceAccount,
        credential: &'a CloudCredential,
        cancellation: &'a dyn CloudRequestFence,
        token: &'a CloudWorkToken,
    ) -> CloudTransportFuture<'a, CloudProfileTransport>;

    fn avatar<'a>(
        &'a self,
        account: &'a CloudServiceAccount,
        credential: &'a CloudCredential,
        etag: Option<&'a str>,
        cancellation: &'a dyn CloudRequestFence,
        token: &'a CloudWorkToken,
    ) -> CloudTransportFuture<'a, AvatarTransportResult>;
}
