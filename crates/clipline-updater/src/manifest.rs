use semver::Version;
use serde::Deserialize;
use std::cmp::Ordering;
use std::error::Error;
use std::fmt;
use std::time::Duration;
use thiserror::Error;
use url::Url;

use crate::download::is_approved_release_redirect;

pub const MAX_MANIFEST_BYTES: usize = 256 * 1024;
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_MANIFEST_REDIRECTS: usize = 5;

pub const NIGHTLY_UPDATE_ENDPOINT: &str =
    "https://github.com/dain98/clipline/releases/download/nightly/latest.json";
pub const NIGHTLY_STANDALONE_UPDATE_ENDPOINT: &str =
    "https://github.com/dain98/clipline/releases/download/nightly/latest-standalone.json";
pub const STABLE_UPDATE_ENDPOINT: &str =
    "https://github.com/dain98/clipline/releases/latest/download/latest.json";
pub const STABLE_STANDALONE_UPDATE_ENDPOINT: &str =
    "https://github.com/dain98/clipline/releases/latest/download/latest-standalone.json";

pub use clipline_settings::{UpdateChannel, UpdateVariant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdatePolicy {
    pub current_version: Version,
    pub channel: UpdateChannel,
    pub variant: UpdateVariant,
}

impl UpdatePolicy {
    pub const fn new(
        current_version: Version,
        channel: UpdateChannel,
        variant: UpdateVariant,
    ) -> Self {
        Self {
            current_version,
            channel,
            variant,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateManifest {
    pub version: Version,
    pub notes: String,
    pub pub_date: String,
    pub target: UpdateTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateTarget {
    pub signature: String,
    pub url: Url,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateCheck {
    MissingRelease,
    Current,
    Available(UpdateManifest),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ManifestErrorKind {
    TooLarge,
    InvalidJson,
    InvalidVersion,
    VersionNotNewer,
    InvalidPublicationDate,
    MissingSignature,
    InvalidDownloadUrl,
    ChannelMismatch,
    VariantMismatch,
}

/// A deliberately value-free error. Malformed response bodies, URLs, and
/// signatures must never be copied into diagnostics or user-facing failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManifestError {
    kind: ManifestErrorKind,
}

impl ManifestError {
    const fn new(kind: ManifestErrorKind) -> Self {
        Self { kind }
    }

    pub const fn kind(self) -> ManifestErrorKind {
        self.kind
    }
}

impl fmt::Display for ManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self.kind {
            ManifestErrorKind::TooLarge => "update manifest exceeds the size limit",
            ManifestErrorKind::InvalidJson => "update manifest is invalid",
            ManifestErrorKind::InvalidVersion => "update manifest version is invalid",
            ManifestErrorKind::VersionNotNewer => "update manifest is not newer than this build",
            ManifestErrorKind::InvalidPublicationDate => {
                "update manifest publication date is invalid"
            }
            ManifestErrorKind::MissingSignature => "update manifest signature is missing",
            ManifestErrorKind::InvalidDownloadUrl => "update manifest download URL is not approved",
            ManifestErrorKind::ChannelMismatch => {
                "update manifest does not match the selected channel"
            }
            ManifestErrorKind::VariantMismatch => {
                "update manifest does not match the installed variant"
            }
        };
        formatter.write_str(message)
    }
}

impl Error for ManifestError {}

#[derive(Debug, Error)]
pub enum CheckError {
    #[error("the update HTTP client could not be initialized")]
    Client,
    #[error("the update manifest request failed")]
    Request,
    #[error("the update manifest redirect was invalid")]
    InvalidRedirect,
    #[error("the update manifest redirect left the approved release policy")]
    RedirectRejected,
    #[error("the update manifest exceeded the redirect limit")]
    TooManyRedirects,
    #[error("the update manifest server returned HTTP {0}")]
    HttpStatus(u16),
    #[error(transparent)]
    Manifest(#[from] ManifestError),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    version: String,
    notes: String,
    pub_date: String,
    platforms: RawPlatforms,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPlatforms {
    #[serde(rename = "windows-x86_64")]
    windows_x86_64: RawTarget,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTarget {
    signature: String,
    url: String,
}

pub fn parse_update_manifest(
    bytes: &[u8],
    policy: &UpdatePolicy,
) -> Result<UpdateManifest, ManifestError> {
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(ManifestError::new(ManifestErrorKind::TooLarge));
    }

    // Serde's struct visitor rejects duplicate recognized fields. Applying
    // deny_unknown_fields at every level also keeps the accepted schema exact.
    let raw: RawManifest = serde_json::from_slice(bytes)
        .map_err(|_| ManifestError::new(ManifestErrorKind::InvalidJson))?;

    let version = Version::parse(&raw.version)
        .map_err(|_| ManifestError::new(ManifestErrorKind::InvalidVersion))?;
    if version.cmp_precedence(&policy.current_version) != Ordering::Greater {
        return Err(ManifestError::new(ManifestErrorKind::VersionNotNewer));
    }
    if policy.channel == UpdateChannel::Stable && !version.pre.is_empty() {
        return Err(ManifestError::new(ManifestErrorKind::ChannelMismatch));
    }
    if !valid_rfc3339_date(&raw.pub_date) {
        return Err(ManifestError::new(
            ManifestErrorKind::InvalidPublicationDate,
        ));
    }
    if raw.windows_target().signature.trim().is_empty() {
        return Err(ManifestError::new(ManifestErrorKind::MissingSignature));
    }

    let target = raw.platforms.windows_x86_64;
    let url = Url::parse(&target.url)
        .map_err(|_| ManifestError::new(ManifestErrorKind::InvalidDownloadUrl))?;
    validate_release_download_url(&url, &version, policy.channel, policy.variant)?;

    Ok(UpdateManifest {
        version,
        notes: raw.notes,
        pub_date: raw.pub_date,
        target: UpdateTarget {
            signature: target.signature,
            url,
        },
    })
}

/// Fetch and validate one update manifest without exposing response bodies in failures.
///
/// A missing release and a version at or below the current build both mean "no update".
pub async fn check_update(policy: &UpdatePolicy) -> Result<UpdateCheck, CheckError> {
    let client = reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(MANIFEST_TIMEOUT)
        .build()
        .map_err(|_| CheckError::Client)?;
    let mut current = Url::parse(policy.channel.manifest_endpoint(policy.variant))
        .expect("static update endpoint is valid");
    let mut redirects = 0_usize;
    let mut response = loop {
        let response = client
            .get(current.clone())
            .send()
            .await
            .map_err(|_| CheckError::Request)?;
        if !response.status().is_redirection() {
            break response;
        }
        if redirects == MAX_MANIFEST_REDIRECTS {
            return Err(CheckError::TooManyRedirects);
        }
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or(CheckError::InvalidRedirect)?;
        let next = current
            .join(location)
            .map_err(|_| CheckError::InvalidRedirect)?;
        if !is_approved_release_redirect(&current, &next) {
            return Err(CheckError::RedirectRejected);
        }
        current = next;
        redirects += 1;
    };

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(UpdateCheck::MissingRelease);
    }
    if !response.status().is_success() {
        return Err(CheckError::HttpStatus(response.status().as_u16()));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_MANIFEST_BYTES as u64)
    {
        return Err(ManifestError::new(ManifestErrorKind::TooLarge).into());
    }

    let mut bytes = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or(0)
            .min(MAX_MANIFEST_BYTES as u64) as usize,
    );
    while let Some(chunk) = response.chunk().await.map_err(|_| CheckError::Request)? {
        let next_len = bytes
            .len()
            .checked_add(chunk.len())
            .ok_or_else(|| ManifestError::new(ManifestErrorKind::TooLarge))?;
        if next_len > MAX_MANIFEST_BYTES {
            return Err(ManifestError::new(ManifestErrorKind::TooLarge).into());
        }
        bytes.extend_from_slice(&chunk);
    }

    match parse_update_manifest(&bytes, policy) {
        Ok(manifest) => Ok(UpdateCheck::Available(manifest)),
        Err(error) if error.kind() == ManifestErrorKind::VersionNotNewer => {
            Ok(UpdateCheck::Current)
        }
        Err(error) => Err(error.into()),
    }
}

impl RawManifest {
    fn windows_target(&self) -> &RawTarget {
        &self.platforms.windows_x86_64
    }
}

/// Validates the exact GitHub release URL carried by a manifest. The download
/// layer separately validates GitHub's signed release-asset redirect target.
pub fn validate_release_download_url(
    url: &Url,
    version: &Version,
    channel: UpdateChannel,
    variant: UpdateVariant,
) -> Result<(), ManifestError> {
    if url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ManifestError::new(ManifestErrorKind::InvalidDownloadUrl));
    }

    let release = match channel {
        UpdateChannel::Nightly => "nightly".to_owned(),
        UpdateChannel::Stable => format!("v{version}"),
    };
    let filename = installer_filename(version, variant);
    let expected_path = format!("/dain98/clipline/releases/download/{release}/{filename}");
    if url.path() == expected_path {
        return Ok(());
    }

    let regular = installer_filename(version, UpdateVariant::Regular);
    let standalone = installer_filename(version, UpdateVariant::Standalone);
    let selected_release_prefix = format!("/dain98/clipline/releases/download/{release}/");
    if url.path().starts_with(&selected_release_prefix)
        && (url.path().ends_with(&regular) || url.path().ends_with(&standalone))
    {
        Err(ManifestError::new(ManifestErrorKind::VariantMismatch))
    } else if url
        .path()
        .starts_with("/dain98/clipline/releases/download/")
        && (url.path().ends_with(&regular) || url.path().ends_with(&standalone))
    {
        Err(ManifestError::new(ManifestErrorKind::ChannelMismatch))
    } else {
        Err(ManifestError::new(ManifestErrorKind::InvalidDownloadUrl))
    }
}

pub fn installer_filename(version: &Version, variant: UpdateVariant) -> String {
    match variant {
        UpdateVariant::Regular => format!("Clipline_{version}_x64-setup.exe"),
        UpdateVariant::Standalone => {
            format!("Clipline_{version}_x64-standalone-setup.exe")
        }
    }
}

fn valid_rfc3339_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || bytes.get(10) != Some(&b'T')
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return false;
    }

    let Some(year) = decimal(bytes, 0, 4) else {
        return false;
    };
    let Some(month) = decimal(bytes, 5, 2) else {
        return false;
    };
    let Some(day) = decimal(bytes, 8, 2) else {
        return false;
    };
    let Some(hour) = decimal(bytes, 11, 2) else {
        return false;
    };
    let Some(minute) = decimal(bytes, 14, 2) else {
        return false;
    };
    let Some(second) = decimal(bytes, 17, 2) else {
        return false;
    };
    if year == 0
        || !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return false;
    }

    let mut cursor = 19;
    if bytes.get(cursor) == Some(&b'.') {
        cursor += 1;
        let fraction_start = cursor;
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
            cursor += 1;
        }
        if cursor == fraction_start {
            return false;
        }
    }

    match bytes.get(cursor) {
        Some(b'Z') => cursor + 1 == bytes.len(),
        Some(b'+') | Some(b'-') => {
            if cursor + 6 != bytes.len() || bytes.get(cursor + 3) != Some(&b':') {
                return false;
            }
            let offset_hour = decimal(bytes, cursor + 1, 2);
            let offset_minute = decimal(bytes, cursor + 4, 2);
            matches!(offset_hour, Some(0..=23)) && matches!(offset_minute, Some(0..=59))
        }
        _ => false,
    }
}

fn decimal(bytes: &[u8], start: usize, length: usize) -> Option<u32> {
    let digits = bytes.get(start..start.checked_add(length)?)?;
    if !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    digits.iter().try_fold(0_u32, |value, digit| {
        value.checked_mul(10)?.checked_add(u32::from(*digit - b'0'))
    })
}

const fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

const fn is_leap_year(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_date_validation_covers_leap_days_offsets_and_fractional_seconds() {
        assert!(valid_rfc3339_date("2024-02-29T23:59:59Z"));
        assert!(valid_rfc3339_date("2026-08-02T12:34:56.123-07:00"));
        assert!(!valid_rfc3339_date("2026-02-29T00:00:00Z"));
        assert!(!valid_rfc3339_date("2026-08-02 12:34:56Z"));
        assert!(!valid_rfc3339_date("2026-08-02T12:34:60Z"));
    }
}
