//! Frontend-independent osu! account secret and profile ownership.

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use clipline_settings::osu::osu_credential_target_for_operation;
use clipline_settings::{
    OsuAccountGeneration, OsuApiSettings, OsuProfileCas, OsuProfileCasKind,
    MAX_OSU_CLIENT_ID_DIGITS, MAX_OSU_CREDENTIAL_CLEANUP_TARGETS, MAX_OSU_USER_BYTES,
};
use clipline_shell::secret::SecretString;
use serde::de::Visitor;

use crate::osu_http::{
    OsuHttpClient, OsuHttpConfig, OsuHttpError, OsuHttpErrorKind, OsuHttpOwner, OsuRecentFetch,
    OsuRequestFence,
};

pub const MAX_OSU_CLIENT_SECRET_BYTES: usize = 2_560;
pub const MAX_OSU_ACCESS_TOKEN_BYTES: usize = 64 * 1024;
// This gate covers every service instance in the process. Cross-process exclusion is supplied by
// Clipline's authenticated single-instance shell before any account service is constructed.
static OSU_ACCOUNT_OPERATION: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsuSecretError {
    Empty,
    TooLarge { maximum: usize },
}

impl fmt::Display for OsuSecretError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("secret is empty"),
            Self::TooLarge { maximum } => write!(formatter, "secret exceeds {maximum} bytes"),
        }
    }
}

impl Error for OsuSecretError {}

fn bounded_secret(value: String, maximum: usize) -> Result<SecretString, OsuSecretError> {
    // Take zeroizing ownership before canonicalization so both the submitted
    // allocation and the canonical replacement are scrubbed independently.
    let submitted = SecretString::new(value);
    let trimmed = submitted.expose_secret().trim();
    if trimmed.is_empty() {
        return Err(OsuSecretError::Empty);
    }
    if trimmed.len() > maximum {
        return Err(OsuSecretError::TooLarge { maximum });
    }
    Ok(SecretString::new(trimmed.to_owned()))
}

/// Move-only OAuth client secret.
pub struct OsuClientSecret(SecretString);

impl OsuClientSecret {
    pub fn new(value: String) -> Result<Self, OsuSecretError> {
        bounded_secret(value, MAX_OSU_CLIENT_SECRET_BYTES).map(Self)
    }

    #[must_use]
    pub fn expose_secret(&self) -> &str {
        self.0.expose_secret()
    }

    fn from_owned(value: SecretString) -> Result<Self, OsuSecretError> {
        let length = value.expose_secret().len();
        if length == 0 {
            return Err(OsuSecretError::Empty);
        }
        if length > MAX_OSU_CLIENT_SECRET_BYTES {
            return Err(OsuSecretError::TooLarge {
                maximum: MAX_OSU_CLIENT_SECRET_BYTES,
            });
        }
        Ok(Self(value))
    }
}

impl fmt::Debug for OsuClientSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OsuClientSecret([REDACTED])")
    }
}

impl<'de> serde::Deserialize<'de> for OsuClientSecret {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_string(ClientSecretVisitor)
    }
}

/// Move-only OAuth access token.
pub struct OsuAccessToken(SecretString);

impl OsuAccessToken {
    pub fn new(value: String) -> Result<Self, OsuSecretError> {
        bounded_secret(value, MAX_OSU_ACCESS_TOKEN_BYTES).map(Self)
    }

    #[must_use]
    pub fn expose_secret(&self) -> &str {
        self.0.expose_secret()
    }
}

impl fmt::Debug for OsuAccessToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OsuAccessToken([REDACTED])")
    }
}

impl<'de> serde::Deserialize<'de> for OsuAccessToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_string(AccessTokenVisitor)
    }
}

struct ClientSecretVisitor;

impl<'de> Visitor<'de> for ClientSecretVisitor {
    type Value = OsuClientSecret;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "a nonempty osu! client secret up to {MAX_OSU_CLIENT_SECRET_BYTES} bytes"
        )
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        OsuClientSecret::new(value).map_err(E::custom)
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.visit_string(value.to_owned())
    }
}

struct AccessTokenVisitor;

impl<'de> Visitor<'de> for AccessTokenVisitor {
    type Value = OsuAccessToken;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "a nonempty osu! access token up to {MAX_OSU_ACCESS_TOKEN_BYTES} bytes"
        )
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        OsuAccessToken::new(value).map_err(E::custom)
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.visit_string(value.to_owned())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsuCredentialPortError {
    Unavailable,
}

pub trait OsuCredentialPort: Send + Sync {
    fn read(&self, target: &str) -> Result<Option<OsuClientSecret>, OsuCredentialPortError>;
    fn write(
        &self,
        target: &str,
        username: &str,
        secret: &OsuClientSecret,
    ) -> Result<(), OsuCredentialPortError>;
    fn delete(&self, target: &str) -> Result<(), OsuCredentialPortError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsuSettingsPortError {
    Stale,
    GenerationExhausted,
    Unavailable,
}

pub trait OsuSettingsPort: Send + Sync {
    fn load(&self) -> Result<OsuApiSettings, OsuSettingsPortError>;
    fn compare_exchange(
        &self,
        change: OsuProfileCas,
    ) -> Result<OsuApiSettings, OsuSettingsPortError>;
}

pub struct OsuSaveRequest {
    pub client_id: String,
    pub user: String,
    pub client_secret: Option<OsuClientSecret>,
}

impl fmt::Debug for OsuSaveRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OsuSaveRequest")
            .field("client_id", &self.client_id)
            .field("user", &self.user)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsuAccountStatus {
    pub account_generation: OsuAccountGeneration,
    pub configured: bool,
    pub secret_present: bool,
    pub client_id: Option<String>,
    pub user: Option<String>,
    pub username: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsuAccountTestResult {
    pub status: OsuAccountStatus,
    pub score_count: usize,
    pub failed_count: usize,
    pub started_at_count: usize,
    pub ended_at_count: usize,
    pub pagination_ceiling_reached: bool,
}

pub type OsuAccountTestFuture<'a> =
    Pin<Box<dyn Future<Output = Result<OsuRecentFetch, OsuHttpError>> + Send + 'a>>;

pub trait OsuAccountTestPort: Send + Sync {
    fn test<'a>(
        &'a self,
        config: OsuHttpConfig,
        fence: &'a dyn OsuRequestFence,
    ) -> OsuAccountTestFuture<'a>;
}

impl OsuAccountTestPort for OsuHttpClient {
    fn test<'a>(
        &'a self,
        config: OsuHttpConfig,
        fence: &'a dyn OsuRequestFence,
    ) -> OsuAccountTestFuture<'a> {
        Box::pin(async move { self.fetch_recent_scores(&config, None, fence).await })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsuAccountServiceError {
    InvalidClientId,
    InvalidUser,
    GenerationExhausted,
    MissingSecret,
    CredentialRead,
    CredentialWrite,
    CredentialTargetCollision,
    CredentialDelete,
    StaleProfile,
    SettingsUnavailable,
    CleanupCapacity,
    RollbackIncomplete,
    Http(OsuHttpErrorKind),
}

impl fmt::Display for OsuAccountServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidClientId => formatter.write_str("osu! client id is invalid"),
            Self::InvalidUser => formatter.write_str("osu! user is invalid"),
            Self::GenerationExhausted => {
                formatter.write_str("osu! account generation is exhausted")
            }
            Self::MissingSecret => formatter.write_str("osu! client secret is missing"),
            Self::CredentialRead => formatter.write_str("read osu! client secret failed"),
            Self::CredentialWrite => formatter.write_str("store osu! client secret failed"),
            Self::CredentialTargetCollision => {
                formatter.write_str("reserve osu! credential target failed")
            }
            Self::CredentialDelete => formatter.write_str("delete osu! client secret failed"),
            Self::StaleProfile => formatter.write_str("osu! account changed during the operation"),
            Self::SettingsUnavailable => formatter.write_str("osu! settings are unavailable"),
            Self::CleanupCapacity => formatter.write_str("osu! credential cleanup queue is full"),
            Self::RollbackIncomplete => {
                formatter.write_str("osu! credential rollback requires cleanup")
            }
            Self::Http(kind) => write!(formatter, "osu! account test failed: {kind:?}"),
        }
    }
}

impl Error for OsuAccountServiceError {}

pub struct OsuAccountService<C, S> {
    credentials: C,
    settings: S,
}

impl<C, S> OsuAccountService<C, S>
where
    C: OsuCredentialPort,
    S: OsuSettingsPort,
{
    #[must_use]
    pub fn new(credentials: C, settings: S) -> Self {
        Self {
            credentials,
            settings,
        }
    }

    pub fn status(&self) -> Result<OsuAccountStatus, OsuAccountServiceError> {
        let _operation = OSU_ACCOUNT_OPERATION
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let loaded = self.load_profile()?;
        // Cleanup remains durably scheduled, but a transient credential-store
        // delete failure must not hide an otherwise usable account status.
        let profile = self.reconcile_cleanup(&loaded).unwrap_or(loaded);
        self.status_for(&profile)
    }

    pub fn save(
        &self,
        request: OsuSaveRequest,
    ) -> Result<OsuAccountStatus, OsuAccountServiceError> {
        let _operation = OSU_ACCOUNT_OPERATION
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let loaded = self.load_profile()?;
        let expected = self.reconcile_cleanup(&loaded)?;
        let next_generation = expected
            .account_generation
            .checked_next()
            .map_err(|_| OsuAccountServiceError::GenerationExhausted)?;
        let client_id = normalize_client_id(request.client_id)?;
        let user = normalize_user(request.user)?;
        let operation_id = uuid::Uuid::new_v4().simple().to_string();
        let target = osu_credential_target_for_operation(next_generation, &operation_id)
            .map_err(|_| OsuAccountServiceError::SettingsUnavailable)?;
        if expected
            .credential_target
            .as_ref()
            .is_some_and(|old_target| {
                old_target != &target
                    && !expected.credential_cleanup_targets.contains(old_target)
                    && expected.credential_cleanup_targets.len()
                        >= MAX_OSU_CREDENTIAL_CLEANUP_TARGETS
            })
        {
            return Err(OsuAccountServiceError::CleanupCapacity);
        }

        // A candidate is unique to this operation. Any collision is preserved
        // rather than overwritten or deleted because it may belong to a
        // separately constructed service or process.
        if self.read_secret(&target)?.is_some() {
            return Err(OsuAccountServiceError::CredentialTargetCollision);
        }
        let secret = match request.client_secret {
            Some(secret) => secret,
            None => {
                let old_target = expected
                    .credential_target
                    .as_deref()
                    .ok_or(OsuAccountServiceError::MissingSecret)?;
                self.read_secret(old_target)?
                    .ok_or(OsuAccountServiceError::MissingSecret)?
            }
        };
        // Persist the cleanup owner before the external write. A crash, a
        // partial credential-store failure, or a rejected profile CAS can
        // therefore never leave an undiscoverable candidate behind.
        let mut reserved = expected.clone();
        push_cleanup_target(&mut reserved, target.clone())?;
        reserved.normalize();
        let reserved = self
            .settings
            .compare_exchange(OsuProfileCas {
                kind: OsuProfileCasKind::Reconcile,
                expected,
                replacement: reserved,
            })
            .map_err(map_settings_error)?;
        if self.credentials.write(&target, &user, &secret).is_err() {
            let _ = self.reconcile_cleanup(&reserved);
            return Err(OsuAccountServiceError::CredentialWrite);
        }

        let mut replacement = reserved.clone();
        replacement.account_generation = next_generation;
        replacement.client_id = Some(client_id);
        replacement.user = Some(user);
        replacement.credential_target = Some(target.clone());
        replacement
            .credential_cleanup_targets
            .retain(|candidate| candidate != &target);
        if let Some(old_target) = reserved
            .credential_target
            .as_ref()
            .filter(|old_target| *old_target != &target)
        {
            push_cleanup_target(&mut replacement, old_target.clone())?;
        }
        if replacement.client_id != reserved.client_id || replacement.user != reserved.user {
            replacement.last_connected_username = None;
        }
        replacement.normalize();

        let profile = match self.settings.compare_exchange(OsuProfileCas {
            kind: OsuProfileCasKind::Save,
            expected: reserved,
            replacement,
        }) {
            Ok(profile) => profile,
            Err(primary) => {
                if let Ok(current) = self.load_profile() {
                    if current.credential_target.as_deref() != Some(target.as_str()) {
                        if !current.credential_cleanup_targets.contains(&target)
                            && self.schedule_cleanup(target.clone()).is_err()
                        {
                            return Err(OsuAccountServiceError::RollbackIncomplete);
                        }
                        if let Ok(scheduled) = self.load_profile() {
                            let _ = self.reconcile_cleanup(&scheduled);
                        }
                    }
                }
                return Err(map_settings_error(primary));
            }
        };
        let _ = self.reconcile_cleanup(&profile);
        self.status_for(&profile)
    }

    pub fn disconnect(&self) -> Result<OsuAccountStatus, OsuAccountServiceError> {
        let _operation = OSU_ACCOUNT_OPERATION
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let expected = self.load_profile()?;
        let next_generation = expected
            .account_generation
            .checked_next()
            .map_err(|_| OsuAccountServiceError::GenerationExhausted)?;
        let mut replacement = OsuApiSettings {
            account_generation: next_generation,
            credential_cleanup_targets: expected.credential_cleanup_targets.clone(),
            ..OsuApiSettings::default()
        };
        if let Some(target) = expected.credential_target.as_ref() {
            push_cleanup_target(&mut replacement, target.clone())?;
        }
        replacement.normalize();
        let committed = self
            .settings
            .compare_exchange(OsuProfileCas {
                kind: OsuProfileCasKind::Disconnect,
                expected,
                replacement,
            })
            .map_err(map_settings_error)?;
        let reconciled = self.reconcile_cleanup(&committed)?;
        self.status_for(&reconciled)
    }

    /// Verify the configured account without holding the settings transaction
    /// gate across network I/O, then advance the exact durable account owner.
    /// A concurrent save/test/disconnect makes the final CAS fail stale.
    pub async fn test<H: OsuAccountTestPort, F: OsuRequestFence>(
        &self,
        http: &H,
        fence: &F,
    ) -> Result<OsuAccountTestResult, OsuAccountServiceError> {
        let (expected_owner, config) = {
            let _operation = OSU_ACCOUNT_OPERATION
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let loaded = self.load_profile()?;
            let expected = self.reconcile_cleanup(&loaded)?;
            let client_id = expected
                .client_id
                .clone()
                .ok_or(OsuAccountServiceError::InvalidClientId)?;
            let user = expected
                .user
                .clone()
                .ok_or(OsuAccountServiceError::InvalidUser)?;
            let target = expected
                .credential_target
                .as_deref()
                .ok_or(OsuAccountServiceError::MissingSecret)?;
            let secret = self
                .read_secret(target)?
                .ok_or(OsuAccountServiceError::MissingSecret)?;
            let config = OsuHttpConfig::new(
                OsuHttpOwner::new(expected.account_generation),
                client_id,
                user,
                secret,
            )
            .map_err(map_http_error)?;
            (expected, config)
        };

        let fetch = http.test(config, fence).await.map_err(map_http_error)?;
        if fetch.owner.account_generation != expected_owner.account_generation {
            return Err(OsuAccountServiceError::StaleProfile);
        }

        let _operation = OSU_ACCOUNT_OPERATION
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let loaded = self.load_profile()?;
        if !same_osu_account_owner(&loaded, &expected_owner) {
            return Err(OsuAccountServiceError::StaleProfile);
        }
        let expected = self.reconcile_cleanup(&loaded)?;
        let next_generation = expected
            .account_generation
            .checked_next()
            .map_err(|_| OsuAccountServiceError::GenerationExhausted)?;
        let client_id = expected
            .client_id
            .clone()
            .ok_or(OsuAccountServiceError::InvalidClientId)?;
        let user_id = normalize_user(fetch.user_id.clone())?;
        let old_target = expected
            .credential_target
            .clone()
            .ok_or(OsuAccountServiceError::MissingSecret)?;
        let secret = self
            .read_secret(&old_target)?
            .ok_or(OsuAccountServiceError::MissingSecret)?;
        let operation_id = uuid::Uuid::new_v4().simple().to_string();
        let target = osu_credential_target_for_operation(next_generation, &operation_id)
            .map_err(|_| OsuAccountServiceError::SettingsUnavailable)?;
        if expected.credential_cleanup_targets.len() >= MAX_OSU_CREDENTIAL_CLEANUP_TARGETS {
            return Err(OsuAccountServiceError::CleanupCapacity);
        }
        if self.read_secret(&target)?.is_some() {
            return Err(OsuAccountServiceError::CredentialTargetCollision);
        }

        let mut reserved = expected.clone();
        push_cleanup_target(&mut reserved, target.clone())?;
        reserved.normalize();
        let reserved = self
            .settings
            .compare_exchange(OsuProfileCas {
                kind: OsuProfileCasKind::Reconcile,
                expected,
                replacement: reserved,
            })
            .map_err(map_settings_error)?;
        if self.credentials.write(&target, &user_id, &secret).is_err() {
            let _ = self.reconcile_cleanup(&reserved);
            return Err(OsuAccountServiceError::CredentialWrite);
        }

        let mut replacement = reserved.clone();
        replacement.account_generation = next_generation;
        replacement.client_id = Some(client_id);
        replacement.user = Some(user_id);
        replacement.credential_target = Some(target.clone());
        replacement.last_connected_username = fetch.username.clone();
        replacement
            .credential_cleanup_targets
            .retain(|candidate| candidate != &target);
        if old_target != target {
            push_cleanup_target(&mut replacement, old_target)?;
        }
        replacement.normalize();
        let profile = match self.settings.compare_exchange(OsuProfileCas {
            kind: OsuProfileCasKind::Test,
            expected: reserved,
            replacement,
        }) {
            Ok(profile) => profile,
            Err(primary) => {
                if let Ok(current) = self.load_profile() {
                    if current.credential_target.as_deref() != Some(target.as_str()) {
                        if !current.credential_cleanup_targets.contains(&target)
                            && self.schedule_cleanup(target.clone()).is_err()
                        {
                            return Err(OsuAccountServiceError::RollbackIncomplete);
                        }
                        if let Ok(scheduled) = self.load_profile() {
                            let _ = self.reconcile_cleanup(&scheduled);
                        }
                    }
                }
                return Err(map_settings_error(primary));
            }
        };
        let profile = self.reconcile_cleanup(&profile).unwrap_or(profile);
        let status = self.status_for(&profile)?;
        Ok(OsuAccountTestResult {
            status,
            score_count: fetch.scores.len(),
            failed_count: fetch.failed_count,
            started_at_count: fetch.started_at_count,
            ended_at_count: fetch.ended_at_count,
            pagination_ceiling_reached: fetch.pagination_ceiling_reached,
        })
    }

    fn load_profile(&self) -> Result<OsuApiSettings, OsuAccountServiceError> {
        self.settings.load().map_err(map_settings_error)
    }

    fn read_secret(&self, target: &str) -> Result<Option<OsuClientSecret>, OsuAccountServiceError> {
        self.credentials
            .read(target)
            .map_err(|_| OsuAccountServiceError::CredentialRead)
    }

    fn status_for(
        &self,
        profile: &OsuApiSettings,
    ) -> Result<OsuAccountStatus, OsuAccountServiceError> {
        let secret_present = match profile.credential_target.as_deref() {
            Some(target) => self.read_secret(target)?.is_some(),
            None => false,
        };
        Ok(OsuAccountStatus {
            account_generation: profile.account_generation,
            configured: profile.client_id.is_some()
                && profile.user.is_some()
                && profile.credential_target.is_some()
                && secret_present,
            secret_present,
            client_id: profile.client_id.clone(),
            user: profile.user.clone(),
            username: profile.last_connected_username.clone(),
        })
    }

    fn reconcile_cleanup(
        &self,
        profile: &OsuApiSettings,
    ) -> Result<OsuApiSettings, OsuAccountServiceError> {
        if profile.credential_cleanup_targets.is_empty() {
            return Ok(profile.clone());
        }
        let mut deleted = Vec::new();
        deleted
            .try_reserve_exact(profile.credential_cleanup_targets.len())
            .map_err(|_| OsuAccountServiceError::SettingsUnavailable)?;
        for target in &profile.credential_cleanup_targets {
            self.credentials
                .delete(target)
                .map_err(|_| OsuAccountServiceError::CredentialDelete)?;
            deleted.push(target.clone());
        }
        let mut replacement = profile.clone();
        replacement
            .credential_cleanup_targets
            .retain(|target| !deleted.contains(target));
        self.settings
            .compare_exchange(OsuProfileCas {
                kind: OsuProfileCasKind::Reconcile,
                expected: profile.clone(),
                replacement,
            })
            .map_err(map_settings_error)
    }

    fn schedule_cleanup(&self, target: String) -> Result<(), OsuAccountServiceError> {
        for _ in 0..3 {
            let current = self.load_profile()?;
            if current.credential_target.as_deref() == Some(target.as_str()) {
                return Err(OsuAccountServiceError::RollbackIncomplete);
            }
            if current.credential_cleanup_targets.contains(&target) {
                return Ok(());
            }
            let mut replacement = current.clone();
            push_cleanup_target(&mut replacement, target.clone())?;
            replacement.normalize();
            match self.settings.compare_exchange(OsuProfileCas {
                kind: OsuProfileCasKind::Reconcile,
                expected: current,
                replacement,
            }) {
                Ok(_) => return Ok(()),
                Err(OsuSettingsPortError::Stale) => continue,
                Err(error) => return Err(map_settings_error(error)),
            }
        }
        Err(OsuAccountServiceError::RollbackIncomplete)
    }
}

fn normalize_client_id(value: String) -> Result<String, OsuAccountServiceError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > MAX_OSU_CLIENT_ID_DIGITS
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || value.parse::<u64>().is_err()
    {
        return Err(OsuAccountServiceError::InvalidClientId);
    }
    Ok(value.to_owned())
}

fn normalize_user(value: String) -> Result<String, OsuAccountServiceError> {
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_OSU_USER_BYTES {
        return Err(OsuAccountServiceError::InvalidUser);
    }
    Ok(value.to_owned())
}

fn push_cleanup_target(
    profile: &mut OsuApiSettings,
    target: String,
) -> Result<(), OsuAccountServiceError> {
    if profile.credential_cleanup_targets.contains(&target) {
        return Ok(());
    }
    if profile.credential_cleanup_targets.len() >= MAX_OSU_CREDENTIAL_CLEANUP_TARGETS {
        return Err(OsuAccountServiceError::CleanupCapacity);
    }
    profile.credential_cleanup_targets.push(target);
    Ok(())
}

fn map_settings_error(error: OsuSettingsPortError) -> OsuAccountServiceError {
    match error {
        OsuSettingsPortError::Stale => OsuAccountServiceError::StaleProfile,
        OsuSettingsPortError::GenerationExhausted => OsuAccountServiceError::GenerationExhausted,
        OsuSettingsPortError::Unavailable => OsuAccountServiceError::SettingsUnavailable,
    }
}

fn map_http_error(error: OsuHttpError) -> OsuAccountServiceError {
    match error.kind() {
        OsuHttpErrorKind::AccountChanged | OsuHttpErrorKind::Canceled => {
            OsuAccountServiceError::StaleProfile
        }
        kind => OsuAccountServiceError::Http(kind),
    }
}

fn same_osu_account_owner(current: &OsuApiSettings, expected: &OsuApiSettings) -> bool {
    current.account_generation == expected.account_generation
        && current.client_id == expected.client_id
        && current.user == expected.user
        && current.credential_target == expected.credential_target
}

impl OsuSettingsPort for clipline_settings::SettingsStore {
    fn load(&self) -> Result<OsuApiSettings, OsuSettingsPortError> {
        self.current_osu_profile()
            .map_err(|_| OsuSettingsPortError::Unavailable)
    }

    fn compare_exchange(
        &self,
        change: OsuProfileCas,
    ) -> Result<OsuApiSettings, OsuSettingsPortError> {
        self.compare_exchange_osu_profile(change)
            .map(|snapshot| snapshot.document.osu)
            .map_err(|error| match error {
                clipline_settings::SettingsTransactionError::StaleOsuProfile => {
                    OsuSettingsPortError::Stale
                }
                clipline_settings::SettingsTransactionError::OsuAccountGenerationExhausted => {
                    OsuSettingsPortError::GenerationExhausted
                }
                _ => OsuSettingsPortError::Unavailable,
            })
    }
}

#[cfg(windows)]
#[derive(Clone, Copy)]
pub struct WindowsOsuCredentialPort {
    store: clipline_shell::windows::credential::CredentialStore,
}

#[cfg(windows)]
impl WindowsOsuCredentialPort {
    #[must_use]
    pub const fn new(value_label: &'static str) -> Self {
        Self {
            store: clipline_shell::windows::credential::CredentialStore::new(value_label),
        }
    }
}

#[cfg(windows)]
impl OsuCredentialPort for WindowsOsuCredentialPort {
    fn read(&self, target: &str) -> Result<Option<OsuClientSecret>, OsuCredentialPortError> {
        self.store
            .read_secret_if_present(target)
            .map_err(|_| OsuCredentialPortError::Unavailable)?
            .map(OsuClientSecret::from_owned)
            .transpose()
            .map_err(|_| OsuCredentialPortError::Unavailable)
    }

    fn write(
        &self,
        target: &str,
        username: &str,
        secret: &OsuClientSecret,
    ) -> Result<(), OsuCredentialPortError> {
        self.store
            .write(target, username, secret.expose_secret())
            .map_err(|_| OsuCredentialPortError::Unavailable)
    }

    fn delete(&self, target: &str) -> Result<(), OsuCredentialPortError> {
        self.store
            .delete_if_present(target)
            .map_err(|_| OsuCredentialPortError::Unavailable)
    }
}
