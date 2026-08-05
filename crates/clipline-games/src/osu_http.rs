//! Bounded, account-fenced osu! HTTP transport.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use chrono::DateTime;
use reqwest::{StatusCode, Url};
use serde::de::{DeserializeOwned, SeqAccess, Visitor};
use serde::Deserialize;
use zeroize::Zeroizing;

use crate::osu::{OsuAccessToken, OsuClientSecret};
use clipline_settings::{OsuAccountGeneration, MAX_OSU_CLIENT_ID_DIGITS, MAX_OSU_USER_BYTES};

pub const OSU_RECENT_PAGE_LIMIT: usize = 100;
pub const OSU_RECENT_SCORE_CEILING: usize = 500;
pub const OSU_CONTROL_JSON_MAX_BYTES: usize = 4 * 1024 * 1024;
pub const OSU_ERROR_BODY_MAX_BYTES: usize = 64 * 1024;
pub const OSU_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_OSU_SCORE_TEXT_BYTES: usize = 4 * 1_024;
pub const MAX_OSU_SCORE_URL_BYTES: usize = 8 * 1_024;
pub const MAX_OSU_SCORE_ID_BYTES: usize = 64;
pub const MAX_OSU_SCORE_MODS: usize = 64;
pub const MAX_OSU_MOD_BYTES: usize = 32;
pub const MAX_OSU_USERNAME_BYTES: usize = MAX_OSU_USER_BYTES;
pub const MAX_OSU_SCORE_RETAINED_BYTES: usize = 32 * 1_024;
pub const MAX_OSU_FETCH_RETAINED_BYTES: usize = 4 * 1024 * 1024;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(15);
const OSU_API_VERSION: &str = "20220705";
const OSU_RECENT_MODE: &str = "osu";
const SCORE_WINDOW_SKEW_SECONDS: i64 = 5;
const PAGE_LIMIT_DESERIALIZE_ERROR: &str = "clipline-osu-page-limit";
const PAGE_ALLOCATION_DESERIALIZE_ERROR: &str = "clipline-osu-page-allocation";

const _: () = assert!(OSU_RECENT_PAGE_LIMIT > 0);
const _: () = assert!(OSU_RECENT_SCORE_CEILING >= OSU_RECENT_PAGE_LIMIT);
const _: () = assert!(OSU_RECENT_SCORE_CEILING.is_multiple_of(OSU_RECENT_PAGE_LIMIT));

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OsuHttpOwner {
    pub account_generation: OsuAccountGeneration,
}

impl OsuHttpOwner {
    #[must_use]
    pub const fn new(account_generation: OsuAccountGeneration) -> Self {
        Self { account_generation }
    }
}

pub struct OsuHttpConfig {
    owner: OsuHttpOwner,
    client_id: String,
    user: String,
    client_secret: OsuClientSecret,
}

impl OsuHttpConfig {
    pub fn new(
        owner: OsuHttpOwner,
        client_id: String,
        user: String,
        client_secret: OsuClientSecret,
    ) -> Result<Self, OsuHttpError> {
        let client_id = client_id.trim();
        if client_id.is_empty()
            || client_id.len() > MAX_OSU_CLIENT_ID_DIGITS
            || !client_id.bytes().all(|byte| byte.is_ascii_digit())
            || client_id.parse::<u64>().is_err()
        {
            return Err(OsuHttpError::new(
                OsuHttpErrorKind::InvalidInput,
                "validate osu! client id",
            ));
        }
        let user = user.trim();
        if user.is_empty() || user.len() > MAX_OSU_USER_BYTES {
            return Err(OsuHttpError::new(
                OsuHttpErrorKind::InvalidInput,
                "validate osu! user",
            ));
        }
        Ok(Self {
            owner,
            client_id: client_id.to_owned(),
            user: user.to_owned(),
            client_secret,
        })
    }

    #[must_use]
    pub const fn owner(&self) -> OsuHttpOwner {
        self.owner
    }
}

impl std::fmt::Debug for OsuHttpConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OsuHttpConfig")
            .field("owner", &self.owner)
            .field("client_id", &self.client_id)
            .field("user", &self.user)
            .field("client_secret", &"[REDACTED]")
            .finish()
    }
}

pub type OsuCancellationFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

pub trait OsuRequestFence: Send + Sync {
    fn is_current(&self, owner: OsuHttpOwner) -> bool;

    fn cancelled<'a>(&'a self, _owner: OsuHttpOwner) -> OsuCancellationFuture<'a> {
        Box::pin(std::future::pending())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsuHttpErrorKind {
    InvalidInput,
    Offline,
    Timeout,
    Unauthorized,
    Remote,
    Malformed,
    TooLarge,
    Canceled,
    AccountChanged,
    Allocation,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{context}")]
pub struct OsuHttpError {
    kind: OsuHttpErrorKind,
    context: &'static str,
    status: Option<u16>,
}

impl OsuHttpError {
    /// Build a typed failure for an alternate bounded transport implementation.
    #[must_use]
    pub const fn new(kind: OsuHttpErrorKind, context: &'static str) -> Self {
        Self {
            kind,
            context,
            status: None,
        }
    }

    const fn status(kind: OsuHttpErrorKind, context: &'static str, status: u16) -> Self {
        Self {
            kind,
            context,
            status: Some(status),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> OsuHttpErrorKind {
        self.kind
    }

    #[must_use]
    pub const fn status_code(&self) -> Option<u16> {
        self.status
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OsuProxyScore {
    pub id: String,
    pub url: Option<String>,
    pub beatmap_id: Option<u32>,
    pub beatmapset_id: Option<u32>,
    pub cover_url: Option<String>,
    pub title: String,
    pub artist: String,
    pub difficulty: String,
    pub mapper: Option<String>,
    pub star_rating: Option<f64>,
    pub mods: Vec<String>,
    pub rank: Option<String>,
    pub passed: bool,
    pub accuracy: Option<f64>,
    pub max_combo: Option<u32>,
    pub total_score: Option<u64>,
    pub pp: Option<f64>,
    pub started_at_unix: Option<i64>,
    pub ended_at_unix: i64,
    pub beatmap_total_length_s: Option<f64>,
}

impl OsuProxyScore {
    pub fn validate_bounds(&self) -> Result<(), OsuHttpError> {
        check_required_text(&self.id, MAX_OSU_SCORE_ID_BYTES, "validate osu! score id")?;
        check_required_text(
            &self.title,
            MAX_OSU_SCORE_TEXT_BYTES,
            "validate osu! score title",
        )?;
        check_required_text(
            &self.artist,
            MAX_OSU_SCORE_TEXT_BYTES,
            "validate osu! score artist",
        )?;
        check_required_text(
            &self.difficulty,
            MAX_OSU_SCORE_TEXT_BYTES,
            "validate osu! score difficulty",
        )?;
        check_optional_text(
            self.url.as_deref(),
            MAX_OSU_SCORE_URL_BYTES,
            "validate osu! score url",
        )?;
        check_optional_text(
            self.cover_url.as_deref(),
            MAX_OSU_SCORE_URL_BYTES,
            "validate osu! score cover url",
        )?;
        check_optional_text(
            self.mapper.as_deref(),
            MAX_OSU_SCORE_TEXT_BYTES,
            "validate osu! score mapper",
        )?;
        check_optional_text(
            self.rank.as_deref(),
            MAX_OSU_SCORE_TEXT_BYTES,
            "validate osu! score rank",
        )?;
        if self.mods.len() > MAX_OSU_SCORE_MODS
            || self
                .mods
                .iter()
                .any(|value| value.is_empty() || value.len() > MAX_OSU_MOD_BYTES)
        {
            return Err(OsuHttpError::new(
                OsuHttpErrorKind::TooLarge,
                "validate osu! score mods",
            ));
        }
        for value in [
            self.star_rating,
            self.accuracy,
            self.pp,
            self.beatmap_total_length_s,
        ]
        .into_iter()
        .flatten()
        {
            if !value.is_finite() {
                return Err(OsuHttpError::new(
                    OsuHttpErrorKind::Malformed,
                    "validate osu! score number",
                ));
            }
        }
        if self.retained_utf8_bytes()? > MAX_OSU_SCORE_RETAINED_BYTES {
            return Err(OsuHttpError::new(
                OsuHttpErrorKind::TooLarge,
                "validate retained osu! score bytes",
            ));
        }
        Ok(())
    }

    fn retained_utf8_bytes(&self) -> Result<usize, OsuHttpError> {
        let fixed = [
            Some(self.id.as_str()),
            self.url.as_deref(),
            self.cover_url.as_deref(),
            Some(self.title.as_str()),
            Some(self.artist.as_str()),
            Some(self.difficulty.as_str()),
            self.mapper.as_deref(),
            self.rank.as_deref(),
        ]
        .into_iter()
        .flatten()
        .try_fold(0usize, |total, value| total.checked_add(value.len()))
        .ok_or_else(|| {
            OsuHttpError::new(OsuHttpErrorKind::TooLarge, "sum retained osu! score bytes")
        })?;
        self.mods.iter().try_fold(fixed, |total, value| {
            total.checked_add(value.len()).ok_or_else(|| {
                OsuHttpError::new(
                    OsuHttpErrorKind::TooLarge,
                    "sum retained osu! score mod bytes",
                )
            })
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OsuRecentFetch {
    pub owner: OsuHttpOwner,
    pub user_id: String,
    pub scores: Vec<OsuProxyScore>,
    pub failed_count: usize,
    pub started_at_count: usize,
    pub ended_at_count: usize,
    pub pagination_ceiling_reached: bool,
    pub username: Option<String>,
}

pub struct OsuHttpClient {
    client: reqwest::Client,
    token_url: Url,
    api_base: Url,
    operation_timeout: Duration,
}

impl std::fmt::Debug for OsuHttpClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OsuHttpClient")
            .field("token_url", &self.token_url)
            .field("api_base", &self.api_base)
            .field("operation_timeout", &self.operation_timeout)
            .finish_non_exhaustive()
    }
}

impl OsuHttpClient {
    pub fn production() -> Result<Self, OsuHttpError> {
        let token_url = Url::parse("https://osu.ppy.sh/oauth/token").map_err(|_| {
            OsuHttpError::new(OsuHttpErrorKind::InvalidInput, "build osu! token URL")
        })?;
        let api_base = Url::parse("https://osu.ppy.sh/api/v2/")
            .map_err(|_| OsuHttpError::new(OsuHttpErrorKind::InvalidInput, "build osu! API URL"))?;
        Self::build_with_endpoints(token_url, api_base, OSU_OPERATION_TIMEOUT)
    }

    /// Construct with explicit endpoints for deterministic local transport tests.
    /// Production callers must use [`Self::production`].
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn with_endpoints(
        token_url: Url,
        api_base: Url,
        operation_timeout: Duration,
    ) -> Result<Self, OsuHttpError> {
        Self::build_with_endpoints(token_url, api_base, operation_timeout)
    }

    fn build_with_endpoints(
        token_url: Url,
        api_base: Url,
        operation_timeout: Duration,
    ) -> Result<Self, OsuHttpError> {
        if operation_timeout.is_zero() || api_base.cannot_be_a_base() {
            return Err(OsuHttpError::new(
                OsuHttpErrorKind::InvalidInput,
                "validate osu! HTTP endpoints",
            ));
        }
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| OsuHttpError::new(OsuHttpErrorKind::Offline, "build osu! HTTP client"))?;
        Ok(Self {
            client,
            token_url,
            api_base,
            operation_timeout,
        })
    }

    pub async fn fetch_recent_scores(
        &self,
        config: &OsuHttpConfig,
        stop_before_unix: Option<i64>,
        fence: &dyn OsuRequestFence,
    ) -> Result<OsuRecentFetch, OsuHttpError> {
        checkpoint(fence, config.owner)?;
        tokio::select! {
            biased;
            _ = fence.cancelled(config.owner) => Err(OsuHttpError::new(
                OsuHttpErrorKind::Canceled,
                "cancel osu! operation",
            )),
            result = tokio::time::timeout(
                self.operation_timeout,
                self.fetch_inner(config, stop_before_unix, fence),
            ) => match result {
                Ok(result) => result,
                Err(_) => Err(OsuHttpError::new(
                    OsuHttpErrorKind::Timeout,
                    "time out osu! operation",
                )),
            },
        }
    }

    async fn fetch_inner(
        &self,
        config: &OsuHttpConfig,
        stop_before_unix: Option<i64>,
        fence: &dyn OsuRequestFence,
    ) -> Result<OsuRecentFetch, OsuHttpError> {
        checkpoint(fence, config.owner)?;
        let token = self.request_app_token(config).await?;
        checkpoint(fence, config.owner)?;
        let resolved = self.resolve_user(&token, &config.user).await?;
        checkpoint(fence, config.owner)?;

        let mut scores = Vec::new();
        scores
            .try_reserve_exact(OSU_RECENT_SCORE_CEILING)
            .map_err(|_| {
                OsuHttpError::new(OsuHttpErrorKind::Allocation, "reserve osu! score results")
            })?;
        let mut failed_count = 0usize;
        let mut started_at_count = 0usize;
        let mut ended_at_count = 0usize;
        let mut username = resolved.username;
        let mut pagination_ceiling_reached = false;
        let mut retained_utf8_bytes = 0usize;

        for offset in (0..OSU_RECENT_SCORE_CEILING).step_by(OSU_RECENT_PAGE_LIMIT) {
            checkpoint(fence, config.owner)?;
            let page = self
                .request_recent_page(&token, &resolved.id, offset)
                .await?;
            checkpoint(fence, config.owner)?;
            if page.len() > OSU_RECENT_PAGE_LIMIT {
                return Err(OsuHttpError::new(
                    OsuHttpErrorKind::TooLarge,
                    "validate osu! score page",
                ));
            }
            if page.is_empty() {
                break;
            }
            let page_len = page.len();
            let mut oldest_ended_at = None;
            for raw in page {
                checkpoint(fence, config.owner)?;
                if !raw.passed {
                    failed_count = failed_count.saturating_add(1);
                }
                if raw.started_at.is_some() {
                    started_at_count = started_at_count.saturating_add(1);
                }
                if raw.ended_at.is_some() {
                    ended_at_count = ended_at_count.saturating_add(1);
                }
                if let Some(name) = raw.user.as_ref().and_then(|user| {
                    bounded_optional(user.username.as_deref(), MAX_OSU_USERNAME_BYTES)
                }) {
                    username = Some(name);
                }
                match normalize_score(raw) {
                    Ok(score) => {
                        checkpoint(fence, config.owner)?;
                        if scores.len() >= OSU_RECENT_SCORE_CEILING {
                            return Err(OsuHttpError::new(
                                OsuHttpErrorKind::TooLarge,
                                "retain osu! score results",
                            ));
                        }
                        oldest_ended_at = Some(
                            oldest_ended_at
                                .map(|oldest: i64| oldest.min(score.ended_at_unix))
                                .unwrap_or(score.ended_at_unix),
                        );
                        retained_utf8_bytes = retained_utf8_bytes
                            .checked_add(score.retained_utf8_bytes()?)
                            .ok_or_else(|| {
                                OsuHttpError::new(
                                    OsuHttpErrorKind::TooLarge,
                                    "sum retained osu! fetch bytes",
                                )
                            })?;
                        if retained_utf8_bytes > MAX_OSU_FETCH_RETAINED_BYTES {
                            return Err(OsuHttpError::new(
                                OsuHttpErrorKind::TooLarge,
                                "retain bounded osu! fetch bytes",
                            ));
                        }
                        scores.push(score);
                    }
                    Err(error) => tracing::warn!(
                        event = "osu_recent_score_skipped",
                        kind = ?error.kind()
                    ),
                }
            }
            if page_len < OSU_RECENT_PAGE_LIMIT {
                break;
            }
            if let (Some(stop), Some(oldest)) = (stop_before_unix, oldest_ended_at) {
                if oldest < stop.saturating_sub(SCORE_WINDOW_SKEW_SECONDS) {
                    break;
                }
            }
            if offset + OSU_RECENT_PAGE_LIMIT >= OSU_RECENT_SCORE_CEILING {
                pagination_ceiling_reached = true;
            }
        }
        checkpoint(fence, config.owner)?;
        Ok(OsuRecentFetch {
            owner: config.owner,
            user_id: resolved.id,
            scores,
            failed_count,
            started_at_count,
            ended_at_count,
            pagination_ceiling_reached,
            username,
        })
    }

    async fn request_app_token(
        &self,
        config: &OsuHttpConfig,
    ) -> Result<OsuAccessToken, OsuHttpError> {
        #[derive(Deserialize)]
        struct TokenResponse {
            access_token: OsuAccessToken,
        }

        #[derive(serde::Serialize)]
        struct TokenForm<'a> {
            client_id: &'a str,
            client_secret: &'a str,
            grant_type: &'static str,
            scope: &'static str,
        }

        let response = self
            .client
            .post(self.token_url.clone())
            .form(&TokenForm {
                client_id: &config.client_id,
                client_secret: config.client_secret.expose_secret(),
                grant_type: "client_credentials",
                scope: "public",
            })
            .send()
            .await
            .map_err(|error| classify_transport(error, "request osu! token"))?;
        // reqwest must construct its own request buffer to transmit a form. The
        // first-party owners remain borrowed/zeroizing, and the response body is
        // scrubbed immediately after direct deserialization into OsuAccessToken.
        let token: TokenResponse = response_json(response, "request osu! token").await?;
        Ok(token.access_token)
    }

    async fn resolve_user(
        &self,
        token: &OsuAccessToken,
        user: &str,
    ) -> Result<ResolvedUser, OsuHttpError> {
        if user.bytes().all(|byte| byte.is_ascii_digit()) {
            return Ok(ResolvedUser {
                id: user.to_owned(),
                username: None,
            });
        }
        #[derive(Deserialize)]
        struct UserResponse {
            id: u64,
            #[serde(default)]
            username: Option<String>,
        }

        let mut url = self.api_base.join("users").map_err(|_| {
            OsuHttpError::new(OsuHttpErrorKind::InvalidInput, "build osu! user lookup URL")
        })?;
        url.path_segments_mut()
            .map_err(|_| {
                OsuHttpError::new(OsuHttpErrorKind::InvalidInput, "build osu! user lookup URL")
            })?
            .push(&user_lookup_segment(user))
            .push(OSU_RECENT_MODE);
        let response = self
            .client
            .get(url)
            .bearer_auth(token.expose_secret())
            .header("x-api-version", OSU_API_VERSION)
            .send()
            .await
            .map_err(|error| classify_transport(error, "resolve osu! user"))?;
        let user: UserResponse = response_json(response, "resolve osu! user").await?;
        let username = user
            .username
            .as_deref()
            .and_then(|value| bounded_optional(Some(value), MAX_OSU_USERNAME_BYTES));
        Ok(ResolvedUser {
            id: user.id.to_string(),
            username,
        })
    }

    async fn request_recent_page(
        &self,
        token: &OsuAccessToken,
        user: &str,
        offset: usize,
    ) -> Result<Vec<RawOsuScore>, OsuHttpError> {
        let mut url = self.api_base.join("users").map_err(|_| {
            OsuHttpError::new(OsuHttpErrorKind::InvalidInput, "build osu! recent URL")
        })?;
        url.path_segments_mut()
            .map_err(|_| {
                OsuHttpError::new(OsuHttpErrorKind::InvalidInput, "build osu! recent URL")
            })?
            .push(user)
            .push("scores")
            .push("recent");
        url.query_pairs_mut()
            .append_pair("include_fails", "1")
            .append_pair("legacy_only", "0")
            .append_pair("mode", OSU_RECENT_MODE)
            .append_pair("limit", &OSU_RECENT_PAGE_LIMIT.to_string())
            .append_pair("offset", &offset.to_string());
        let response = self
            .client
            .get(url)
            .bearer_auth(token.expose_secret())
            .header("x-api-version", OSU_API_VERSION)
            .send()
            .await
            .map_err(|error| classify_transport(error, "fetch osu! recent scores"))?;
        response_score_page(response, "fetch osu! recent scores").await
    }
}

#[derive(Debug)]
struct ResolvedUser {
    id: String,
    username: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawOsuScore {
    id: serde_json::Value,
    #[serde(default)]
    beatmap: Option<RawOsuBeatmap>,
    #[serde(default)]
    beatmapset: Option<RawOsuBeatmapset>,
    #[serde(default)]
    mods: Vec<RawOsuMod>,
    #[serde(default)]
    rank: Option<String>,
    #[serde(default)]
    passed: bool,
    #[serde(default)]
    accuracy: Option<f64>,
    #[serde(default)]
    max_combo: Option<u32>,
    #[serde(default)]
    total_score: Option<u64>,
    #[serde(default)]
    score: Option<u64>,
    #[serde(default)]
    pp: Option<f64>,
    #[serde(default)]
    started_at: Option<String>,
    #[serde(default)]
    ended_at: Option<String>,
    #[serde(default)]
    user: Option<RawOsuUser>,
}

struct BoundedRawScorePage(Vec<RawOsuScore>);

impl<'de> Deserialize<'de> for BoundedRawScorePage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_seq(BoundedRawScorePageVisitor)
    }
}

struct BoundedRawScorePageVisitor;

impl<'de> Visitor<'de> for BoundedRawScorePageVisitor {
    type Value = BoundedRawScorePage;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "at most {OSU_RECENT_PAGE_LIMIT} osu! recent scores"
        )
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        if sequence
            .size_hint()
            .is_some_and(|hint| hint > OSU_RECENT_PAGE_LIMIT)
        {
            return Err(serde::de::Error::custom(PAGE_LIMIT_DESERIALIZE_ERROR));
        }
        let mut scores = Vec::new();
        scores
            .try_reserve_exact(sequence.size_hint().unwrap_or(0).min(OSU_RECENT_PAGE_LIMIT))
            .map_err(|_| serde::de::Error::custom(PAGE_ALLOCATION_DESERIALIZE_ERROR))?;
        while let Some(score) = sequence.next_element()? {
            if scores.len() >= OSU_RECENT_PAGE_LIMIT {
                return Err(serde::de::Error::custom(PAGE_LIMIT_DESERIALIZE_ERROR));
            }
            scores.push(score);
        }
        Ok(BoundedRawScorePage(scores))
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawOsuBeatmap {
    #[serde(default)]
    id: Option<u32>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    difficulty_rating: Option<f64>,
    #[serde(default)]
    total_length: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
struct RawOsuBeatmapset {
    #[serde(default)]
    id: Option<u32>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    artist: Option<String>,
    #[serde(default)]
    creator: Option<String>,
    #[serde(default)]
    covers: RawOsuBeatmapsetCovers,
}

#[derive(Debug, Default, Deserialize)]
struct RawOsuBeatmapsetCovers {
    #[serde(default)]
    list: Option<String>,
    #[serde(default)]
    card: Option<String>,
    #[serde(default)]
    cover: Option<String>,
    #[serde(default)]
    slimcover: Option<String>,
    #[serde(default, rename = "list@2x")]
    list_2x: Option<String>,
    #[serde(default, rename = "card@2x")]
    card_2x: Option<String>,
    #[serde(default, rename = "cover@2x")]
    cover_2x: Option<String>,
    #[serde(default, rename = "slimcover@2x")]
    slimcover_2x: Option<String>,
}

impl RawOsuBeatmapsetCovers {
    fn best_rail_cover(self) -> Result<Option<String>, OsuHttpError> {
        for value in [
            self.list,
            self.card,
            self.cover,
            self.slimcover,
            self.list_2x,
            self.card_2x,
            self.cover_2x,
            self.slimcover_2x,
        ]
        .into_iter()
        .flatten()
        {
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            if value.len() > MAX_OSU_SCORE_URL_BYTES {
                return Err(OsuHttpError::new(
                    OsuHttpErrorKind::TooLarge,
                    "validate osu! score cover url",
                ));
            }
            return Ok(Some(value.to_owned()));
        }
        Ok(None)
    }
}

#[derive(Debug, Deserialize)]
struct RawOsuUser {
    #[serde(default)]
    username: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawOsuMod {
    Object { acronym: String },
    Text(String),
}

fn normalize_score(score: RawOsuScore) -> Result<OsuProxyScore, OsuHttpError> {
    if score.mods.len() > MAX_OSU_SCORE_MODS {
        return Err(OsuHttpError::new(
            OsuHttpErrorKind::TooLarge,
            "validate osu! score mods",
        ));
    }
    let id = score_id(score.id)?;
    let ended_at_unix = parse_required_time(score.ended_at.as_deref(), "parse score ended_at")?;
    let started_at_unix = score
        .started_at
        .as_deref()
        .map(|value| parse_required_time(Some(value), "parse score started_at"))
        .transpose()?;
    let beatmap = score.beatmap.unwrap_or_default();
    let beatmapset = score.beatmapset.unwrap_or_default();
    let mut mods = Vec::new();
    mods.try_reserve_exact(score.mods.len())
        .map_err(|_| OsuHttpError::new(OsuHttpErrorKind::Allocation, "reserve osu! score mods"))?;
    for value in score.mods {
        let value = match value {
            RawOsuMod::Object { acronym } | RawOsuMod::Text(acronym) => acronym,
        };
        mods.push(required_bounded_owned(
            value,
            MAX_OSU_MOD_BYTES,
            "validate osu! score mod",
        )?);
    }
    let score = OsuProxyScore {
        url: Some(format!("https://osu.ppy.sh/scores/osu/{id}")),
        id,
        beatmap_id: beatmap.id,
        beatmapset_id: beatmapset.id,
        cover_url: beatmapset.covers.best_rail_cover()?,
        title: bounded_raw_optional(
            beatmapset.title,
            MAX_OSU_SCORE_TEXT_BYTES,
            "validate osu! score title",
        )?
        .unwrap_or_else(|| "Unknown beatmap".into()),
        artist: bounded_raw_optional(
            beatmapset.artist,
            MAX_OSU_SCORE_TEXT_BYTES,
            "validate osu! score artist",
        )?
        .unwrap_or_else(|| "Unknown artist".into()),
        difficulty: bounded_raw_optional(
            beatmap.version,
            MAX_OSU_SCORE_TEXT_BYTES,
            "validate osu! score difficulty",
        )?
        .unwrap_or_else(|| "Unknown difficulty".into()),
        mapper: bounded_raw_optional(
            beatmapset.creator,
            MAX_OSU_SCORE_TEXT_BYTES,
            "validate osu! score mapper",
        )?,
        star_rating: beatmap.difficulty_rating,
        mods,
        rank: bounded_raw_optional(
            score.rank,
            MAX_OSU_SCORE_TEXT_BYTES,
            "validate osu! score rank",
        )?,
        passed: score.passed,
        accuracy: score.accuracy,
        max_combo: score.max_combo,
        total_score: score.total_score.or(score.score),
        pp: score.pp,
        started_at_unix,
        ended_at_unix,
        beatmap_total_length_s: beatmap.total_length,
    };
    score.validate_bounds()?;
    Ok(score)
}

fn score_id(value: serde_json::Value) -> Result<String, OsuHttpError> {
    let value = match value {
        serde_json::Value::Number(number) => number
            .as_u64()
            .map(|value| value.to_string())
            .ok_or_else(|| OsuHttpError::new(OsuHttpErrorKind::Malformed, "parse osu! score id"))?,
        serde_json::Value::String(value) => value,
        _ => {
            return Err(OsuHttpError::new(
                OsuHttpErrorKind::Malformed,
                "parse osu! score id",
            ))
        }
    };
    required_bounded_owned(value, MAX_OSU_SCORE_ID_BYTES, "validate osu! score id")
}

fn parse_required_time(value: Option<&str>, context: &'static str) -> Result<i64, OsuHttpError> {
    let value = value.ok_or_else(|| OsuHttpError::new(OsuHttpErrorKind::Malformed, context))?;
    if value.len() > 64 {
        return Err(OsuHttpError::new(OsuHttpErrorKind::TooLarge, context));
    }
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.timestamp())
        .map_err(|_| OsuHttpError::new(OsuHttpErrorKind::Malformed, context))
}

fn checkpoint(fence: &dyn OsuRequestFence, owner: OsuHttpOwner) -> Result<(), OsuHttpError> {
    if fence.is_current(owner) {
        Ok(())
    } else {
        Err(OsuHttpError::new(
            OsuHttpErrorKind::AccountChanged,
            "fence osu! account operation",
        ))
    }
}

async fn response_json<T: DeserializeOwned>(
    response: reqwest::Response,
    context: &'static str,
) -> Result<T, OsuHttpError> {
    let status = response.status();
    if !status.is_success() {
        let _ = response_bytes_limited(response, OSU_ERROR_BODY_MAX_BYTES, context).await;
        let kind = if status == StatusCode::UNAUTHORIZED {
            OsuHttpErrorKind::Unauthorized
        } else {
            OsuHttpErrorKind::Remote
        };
        return Err(OsuHttpError::status(kind, context, status.as_u16()));
    }
    let bytes = response_bytes_limited(response, OSU_CONTROL_JSON_MAX_BYTES, context).await?;
    serde_json::from_slice(bytes.as_slice())
        .map_err(|_| OsuHttpError::new(OsuHttpErrorKind::Malformed, context))
}

async fn response_score_page(
    response: reqwest::Response,
    context: &'static str,
) -> Result<Vec<RawOsuScore>, OsuHttpError> {
    let status = response.status();
    if !status.is_success() {
        let _ = response_bytes_limited(response, OSU_ERROR_BODY_MAX_BYTES, context).await;
        let kind = if status == StatusCode::UNAUTHORIZED {
            OsuHttpErrorKind::Unauthorized
        } else {
            OsuHttpErrorKind::Remote
        };
        return Err(OsuHttpError::status(kind, context, status.as_u16()));
    }
    let bytes = response_bytes_limited(response, OSU_CONTROL_JSON_MAX_BYTES, context).await?;
    serde_json::from_slice::<BoundedRawScorePage>(bytes.as_slice())
        .map(|page| page.0)
        .map_err(|error| {
            let error = error.to_string();
            if error.contains(PAGE_LIMIT_DESERIALIZE_ERROR) {
                OsuHttpError::new(OsuHttpErrorKind::TooLarge, context)
            } else if error.contains(PAGE_ALLOCATION_DESERIALIZE_ERROR) {
                OsuHttpError::new(OsuHttpErrorKind::Allocation, context)
            } else {
                OsuHttpError::new(OsuHttpErrorKind::Malformed, context)
            }
        })
}

async fn response_bytes_limited(
    mut response: reqwest::Response,
    maximum: usize,
    context: &'static str,
) -> Result<Zeroizing<Vec<u8>>, OsuHttpError> {
    if response
        .content_length()
        .is_some_and(|length| length > maximum as u64)
    {
        return Err(OsuHttpError::new(OsuHttpErrorKind::TooLarge, context));
    }
    let capacity = response.content_length().unwrap_or(0).min(maximum as u64) as usize;
    let mut bytes = Zeroizing::new(Vec::new());
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| OsuHttpError::new(OsuHttpErrorKind::Allocation, "reserve osu! response"))?;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| classify_transport(error, context))?
    {
        let total = bytes
            .len()
            .checked_add(chunk.len())
            .ok_or_else(|| OsuHttpError::new(OsuHttpErrorKind::TooLarge, context))?;
        if total > maximum {
            return Err(OsuHttpError::new(OsuHttpErrorKind::TooLarge, context));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn classify_transport(error: reqwest::Error, context: &'static str) -> OsuHttpError {
    let kind = if error.is_timeout() {
        OsuHttpErrorKind::Timeout
    } else {
        OsuHttpErrorKind::Offline
    };
    OsuHttpError::new(kind, context)
}

fn user_lookup_segment(user: &str) -> String {
    if user.starts_with('@') || user.bytes().all(|byte| byte.is_ascii_digit()) {
        user.to_owned()
    } else {
        format!("@{user}")
    }
}

fn required_bounded_owned(
    value: String,
    maximum: usize,
    context: &'static str,
) -> Result<String, OsuHttpError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(OsuHttpError::new(OsuHttpErrorKind::Malformed, context));
    }
    if value.len() > maximum {
        return Err(OsuHttpError::new(OsuHttpErrorKind::TooLarge, context));
    }
    Ok(value.to_owned())
}

fn bounded_raw_optional(
    value: Option<String>,
    maximum: usize,
    context: &'static str,
) -> Result<Option<String>, OsuHttpError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > maximum {
        return Err(OsuHttpError::new(OsuHttpErrorKind::TooLarge, context));
    }
    Ok(Some(value.to_owned()))
}

fn bounded_optional(value: Option<&str>, maximum: usize) -> Option<String> {
    let value = value?.trim();
    (!value.is_empty() && value.len() <= maximum).then(|| value.to_owned())
}

fn check_required_text(
    value: &str,
    maximum: usize,
    context: &'static str,
) -> Result<(), OsuHttpError> {
    if value.is_empty() {
        return Err(OsuHttpError::new(OsuHttpErrorKind::Malformed, context));
    }
    if value.len() > maximum {
        return Err(OsuHttpError::new(OsuHttpErrorKind::TooLarge, context));
    }
    Ok(())
}

fn check_optional_text(
    value: Option<&str>,
    maximum: usize,
    context: &'static str,
) -> Result<(), OsuHttpError> {
    if value.is_some_and(|value| value.len() > maximum) {
        return Err(OsuHttpError::new(OsuHttpErrorKind::TooLarge, context));
    }
    Ok(())
}
