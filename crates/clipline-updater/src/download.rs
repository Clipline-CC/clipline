//! Bounded, cancellable installer downloads.
//!
//! The manifest layer is expected to validate the initial asset URL. This module
//! validates it again at the network boundary and follows redirects manually so
//! that no redirect can escape Clipline's GitHub release-download policy.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use reqwest::header::LOCATION;
use reqwest::{StatusCode, Url};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::fs::{self, File, OpenOptions};
use tokio::io::AsyncWriteExt;

/// The largest installer Clipline will allocate disk space for or download.
pub const MAX_INSTALLER_BYTES: u64 = 512 * 1024 * 1024;

/// Connection establishment and each idle read have a finite deadline.
///
/// This is deliberately not a total-body deadline: the signed installer is
/// currently about 54 MiB and must remain downloadable on slower links as long
/// as bytes continue to arrive.
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(20);

const MAX_REDIRECTS: usize = 5;
const RELEASE_HOST: &str = "github.com";
const RELEASE_ASSET_HOST: &str = "release-assets.githubusercontent.com";
const RELEASE_OWNER: &str = "dain98";
const RELEASE_REPOSITORY: &str = "clipline";

/// Exact evidence produced after the owned destination is durable on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadTelemetry {
    pub destination: PathBuf,
    pub final_url: Url,
    pub declared_content_length: Option<u64>,
    pub bytes_written: u64,
    pub sha256: [u8; 32],
    pub redirects_followed: usize,
}

impl DownloadTelemetry {
    /// Lowercase SHA-256 text suitable for comparison with release metadata.
    #[must_use]
    pub fn sha256_hex(&self) -> String {
        hex_lower(&self.sha256)
    }
}

/// A typed, body-free failure from the download boundary.
#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("the update download was cancelled")]
    Cancelled,
    #[error("the installer destination already exists")]
    DestinationExists,
    #[error("the installer destination could not be created: {0}")]
    CreateDestination(#[source] io::Error),
    #[error("the installer destination could not be written: {0}")]
    WriteDestination(#[source] io::Error),
    #[error("the installer destination could not be made durable: {0}")]
    SyncDestination(#[source] io::Error),
    #[error("the HTTP client could not be initialized: {0}")]
    Client(#[source] reqwest::Error),
    #[error("the installer request failed: {0}")]
    Request(#[source] reqwest::Error),
    #[error("the installer download exceeded its deadline")]
    Timeout,
    #[error("the installer server returned HTTP {0}")]
    HttpStatus(StatusCode),
    #[error("the installer redirect did not include a valid Location header")]
    InvalidRedirectLocation,
    #[error("the installer redirect left the approved GitHub release-download policy")]
    RedirectRejected,
    #[error("the installer exceeded the redirect limit")]
    TooManyRedirects,
    #[error("the installer declared {declared} bytes, exceeding the {limit}-byte limit")]
    DeclaredLengthTooLarge { declared: u64, limit: u64 },
    #[error("the installer stream exceeded the {limit}-byte limit")]
    StreamTooLarge { limit: u64 },
    #[error("the installer declared {declared} bytes but streamed {actual} bytes")]
    ContentLengthMismatch { declared: u64, actual: u64 },
}

impl DownloadError {
    /// Cancellation is intentionally separate from transport and validation failures.
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

/// Returns whether `url` is an exact Clipline GitHub release asset URL.
///
/// This accepts both a tag-specific `/releases/download/<tag>/<asset>` URL and
/// GitHub's stable `/releases/latest/download/<asset>` spelling. Manifest policy
/// remains responsible for checking the selected channel, variant, and filename.
#[must_use]
pub fn is_approved_release_download_url(url: &Url) -> bool {
    if !has_strict_https_origin(url, RELEASE_HOST) || url.query().is_some() {
        return false;
    }

    let Some(segments) = url.path_segments() else {
        return false;
    };
    let segments = segments.collect::<Vec<_>>();
    if segments.len() != 6
        || segments[0] != RELEASE_OWNER
        || segments[1] != RELEASE_REPOSITORY
        || segments[2] != "releases"
        || segments[5].is_empty()
    {
        return false;
    }

    (segments[3] == "download" && !segments[4].is_empty())
        || (segments[3] == "latest" && segments[4] == "download")
}

/// Returns whether one HTTP redirect remains inside the approved release path.
#[must_use]
pub fn is_approved_release_redirect(previous: &Url, next: &Url) -> bool {
    if !is_approved_redirect_source(previous) {
        return false;
    }

    if is_approved_release_download_url(next) {
        return true;
    }

    has_strict_https_origin(next, RELEASE_ASSET_HOST)
        && next.path().starts_with("/github-production-release-asset/")
}

/// Download one installer into an invocation-owned, previously absent file.
///
/// `cancelled` is checked before destination creation, before every request,
/// before every streamed chunk is written, and before durability is published.
/// The destination is removed on every failure after this invocation creates it.
pub async fn download_installer(
    url: Url,
    destination: impl AsRef<Path>,
    cancelled: &AtomicBool,
) -> Result<DownloadTelemetry, DownloadError> {
    if cancelled.load(Ordering::Acquire) {
        return Err(DownloadError::Cancelled);
    }
    if !is_approved_release_download_url(&url) {
        return Err(DownloadError::RedirectRejected);
    }

    let client = production_client()?;
    download_owned(&client, &url, destination.as_ref(), cancelled).await
}

fn production_client() -> Result<reqwest::Client, DownloadError> {
    reqwest::Client::builder()
        .use_rustls_tls()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(DOWNLOAD_TIMEOUT)
        .read_timeout(DOWNLOAD_TIMEOUT)
        .build()
        .map_err(DownloadError::Client)
}

async fn download_owned(
    client: &reqwest::Client,
    url: &Url,
    destination: &Path,
    cancelled: &AtomicBool,
) -> Result<DownloadTelemetry, DownloadError> {
    let destination = destination.to_path_buf();
    let mut file = create_owned_destination(&destination).await?;
    let result = download_into(client, url, &destination, &mut file, cancelled).await;
    drop(file);

    if result.is_err() {
        // Only this invocation can have created `destination` because creation
        // used `create_new`; never remove a pre-existing caller-owned file.
        let _ = fs::remove_file(&destination).await;
    }
    result
}

async fn create_owned_destination(destination: &Path) -> Result<File, DownloadError> {
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .await
    {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            Err(DownloadError::DestinationExists)
        }
        Err(error) => Err(DownloadError::CreateDestination(error)),
    }
}

async fn download_into(
    client: &reqwest::Client,
    initial_url: &Url,
    destination: &Path,
    file: &mut File,
    cancelled: &AtomicBool,
) -> Result<DownloadTelemetry, DownloadError> {
    let mut current_url = initial_url.clone();
    let mut redirects_followed = 0_usize;
    let mut response = loop {
        check_cancelled(cancelled)?;
        let response = client
            .get(current_url.clone())
            .send()
            .await
            .map_err(classify_request_error)?;

        if !response.status().is_redirection() {
            break response;
        }
        if redirects_followed == MAX_REDIRECTS {
            return Err(DownloadError::TooManyRedirects);
        }

        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or(DownloadError::InvalidRedirectLocation)?;
        let next_url = current_url
            .join(location)
            .map_err(|_| DownloadError::InvalidRedirectLocation)?;
        if !is_approved_release_redirect(&current_url, &next_url) {
            return Err(DownloadError::RedirectRejected);
        }

        current_url = next_url;
        redirects_followed += 1;
    };

    let status = response.status();
    if !status.is_success() {
        // Do not consume or include the response body in an error or log.
        return Err(DownloadError::HttpStatus(status));
    }

    let declared_content_length = response.content_length();
    validate_declared_length(declared_content_length)?;

    let mut hasher = Sha256::new();
    let mut bytes_written = 0_u64;
    while let Some(chunk) = response.chunk().await.map_err(classify_request_error)? {
        check_cancelled(cancelled)?;
        let chunk_len = u64::try_from(chunk.len()).map_err(|_| DownloadError::StreamTooLarge {
            limit: MAX_INSTALLER_BYTES,
        })?;
        let next_len =
            bytes_written
                .checked_add(chunk_len)
                .ok_or(DownloadError::StreamTooLarge {
                    limit: MAX_INSTALLER_BYTES,
                })?;
        if next_len > MAX_INSTALLER_BYTES {
            return Err(DownloadError::StreamTooLarge {
                limit: MAX_INSTALLER_BYTES,
            });
        }

        file.write_all(&chunk)
            .await
            .map_err(DownloadError::WriteDestination)?;
        hasher.update(&chunk);
        bytes_written = next_len;
    }

    check_cancelled(cancelled)?;
    if let Some(declared) = declared_content_length {
        if declared != bytes_written {
            return Err(DownloadError::ContentLengthMismatch {
                declared,
                actual: bytes_written,
            });
        }
    }

    file.sync_all()
        .await
        .map_err(DownloadError::SyncDestination)?;
    check_cancelled(cancelled)?;

    Ok(DownloadTelemetry {
        destination: destination.to_path_buf(),
        final_url: current_url,
        declared_content_length,
        bytes_written,
        sha256: hasher.finalize().into(),
        redirects_followed,
    })
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), DownloadError> {
    if cancelled.load(Ordering::Acquire) {
        Err(DownloadError::Cancelled)
    } else {
        Ok(())
    }
}

fn classify_request_error(error: reqwest::Error) -> DownloadError {
    if error.is_timeout() {
        DownloadError::Timeout
    } else {
        DownloadError::Request(error)
    }
}

fn validate_declared_length(declared: Option<u64>) -> Result<(), DownloadError> {
    match declared {
        Some(declared) if declared > MAX_INSTALLER_BYTES => {
            Err(DownloadError::DeclaredLengthTooLarge {
                declared,
                limit: MAX_INSTALLER_BYTES,
            })
        }
        _ => Ok(()),
    }
}

fn has_strict_https_origin(url: &Url, host: &str) -> bool {
    url.scheme() == "https"
        && url.host_str() == Some(host)
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
}

fn is_approved_redirect_source(url: &Url) -> bool {
    is_approved_release_download_url(url)
        || (has_strict_https_origin(url, RELEASE_ASSET_HOST)
            && url.path().starts_with("/github-production-release-asset/"))
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock after epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "clipline-updater-download-{name}-{}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create test directory");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn approved_url() -> Url {
        Url::parse("https://github.com/dain98/clipline/releases/download/nightly/Clipline.exe")
            .expect("valid URL")
    }

    #[test]
    fn accepts_only_exact_clipline_release_asset_urls() {
        for url in [
            "https://github.com/dain98/clipline/releases/download/nightly/Clipline.exe",
            "https://github.com/dain98/clipline/releases/latest/download/Clipline.exe",
        ] {
            assert!(is_approved_release_download_url(
                &Url::parse(url).expect("valid URL")
            ));
        }

        for url in [
            "http://github.com/dain98/clipline/releases/download/nightly/Clipline.exe",
            "https://evil.example/dain98/clipline/releases/download/nightly/Clipline.exe",
            "https://github.com/other/clipline/releases/download/nightly/Clipline.exe",
            "https://github.com/dain98/clipline/releases/download/nightly/subdir/Clipline.exe",
            "https://github.com/dain98/clipline/releases/download/nightly/Clipline.exe?crossed=1",
            "https://user@github.com/dain98/clipline/releases/download/nightly/Clipline.exe",
        ] {
            assert!(
                !is_approved_release_download_url(&Url::parse(url).expect("valid URL")),
                "unexpectedly accepted {url}"
            );
        }
    }

    #[test]
    fn redirect_policy_accepts_only_github_release_asset_hops() {
        let source =
            Url::parse("https://github.com/dain98/clipline/releases/download/nightly/Clipline.exe")
                .expect("valid source");
        let asset = Url::parse(
            "https://release-assets.githubusercontent.com/github-production-release-asset/123/a?sp=r",
        )
        .expect("valid asset");
        assert!(is_approved_release_redirect(&source, &asset));

        for target in [
            "https://objects.githubusercontent.com/github-production-release-asset/123/a",
            "https://release-assets.githubusercontent.com/unrelated/123/a",
            "http://release-assets.githubusercontent.com/github-production-release-asset/123/a",
            "https://release-assets.githubusercontent.com.evil.example/github-production-release-asset/123/a",
        ] {
            assert!(
                !is_approved_release_redirect(
                    &source,
                    &Url::parse(target).expect("valid target")
                ),
                "unexpectedly followed {target}"
            );
        }
    }

    #[test]
    fn telemetry_hash_text_is_exact_lowercase_sha256() {
        let telemetry = DownloadTelemetry {
            destination: PathBuf::from("owned.part"),
            final_url: Url::parse(
                "https://release-assets.githubusercontent.com/github-production-release-asset/1/a",
            )
            .expect("valid URL"),
            declared_content_length: Some(3),
            bytes_written: 3,
            sha256: Sha256::digest(b"abc").into(),
            redirects_followed: 1,
        };
        assert_eq!(
            telemetry.sha256_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn declared_installer_length_is_bounded_before_streaming() {
        assert!(validate_declared_length(Some(MAX_INSTALLER_BYTES)).is_ok());
        assert!(matches!(
            validate_declared_length(Some(MAX_INSTALLER_BYTES + 1)),
            Err(DownloadError::DeclaredLengthTooLarge {
                declared,
                limit: MAX_INSTALLER_BYTES,
            }) if declared == MAX_INSTALLER_BYTES + 1
        ));
    }

    #[tokio::test]
    async fn cancellation_before_request_creates_no_partial_file() {
        let directory = TestDirectory::new("pre-cancel");
        let destination = directory.0.join("installer.part");
        let cancelled = AtomicBool::new(true);

        let error = download_installer(approved_url(), &destination, &cancelled)
            .await
            .expect_err("cancelled download must fail");

        assert!(error.is_cancelled());
        assert!(!destination.exists());
    }

    #[tokio::test]
    async fn create_new_never_overwrites_a_caller_owned_destination() {
        let directory = TestDirectory::new("no-overwrite");
        let destination = directory.0.join("installer.part");
        std::fs::write(&destination, b"caller-owned").expect("seed destination");
        let cancelled = AtomicBool::new(false);

        let error = download_installer(approved_url(), &destination, &cancelled)
            .await
            .expect_err("existing destination must fail");

        assert!(matches!(error, DownloadError::DestinationExists));
        assert_eq!(
            std::fs::read(destination).expect("read destination"),
            b"caller-owned"
        );
    }

    #[tokio::test]
    async fn streams_exact_bytes_hash_and_length_into_owned_destination() {
        let server = MockServer::start_async().await;
        let payload = b"bounded installer bytes";
        let request = server
            .mock_async(|when, then| {
                when.method(GET).path("/installer");
                then.status(200).body(payload);
            })
            .await;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("test client");
        let directory = TestDirectory::new("success");
        let destination = directory.0.join("installer.part");
        let telemetry = download_owned(
            &client,
            &Url::parse(&server.url("/installer")).expect("test URL"),
            &destination,
            &AtomicBool::new(false),
        )
        .await
        .expect("bounded download");

        request.assert_async().await;
        assert_eq!(std::fs::read(&destination).unwrap(), payload);
        assert_eq!(telemetry.bytes_written, payload.len() as u64);
        assert_eq!(
            telemetry.declared_content_length,
            Some(payload.len() as u64)
        );
        let expected_sha256: [u8; 32] = Sha256::digest(payload).into();
        assert_eq!(telemetry.sha256, expected_sha256);
    }

    #[tokio::test]
    async fn cancellation_after_destination_creation_removes_partial_file() {
        let client = reqwest::Client::new();
        let directory = TestDirectory::new("cancel-owned");
        let destination = directory.0.join("installer.part");
        let error = download_owned(
            &client,
            &Url::parse("http://127.0.0.1:1/never-requested").expect("test URL"),
            &destination,
            &AtomicBool::new(true),
        )
        .await
        .expect_err("post-create cancellation must fail");

        assert!(matches!(error, DownloadError::Cancelled));
        assert!(!destination.exists());
    }

    #[tokio::test]
    async fn timeout_after_destination_creation_is_typed_and_cleans_partial_file() {
        let server = MockServer::start_async().await;
        let request = server
            .mock_async(|when, then| {
                when.method(GET).path("/slow-installer");
                then.status(200)
                    .delay(Duration::from_millis(100))
                    .body("late bytes");
            })
            .await;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(10))
            .build()
            .expect("test client");
        let directory = TestDirectory::new("timeout-owned");
        let destination = directory.0.join("installer.part");
        let error = download_owned(
            &client,
            &Url::parse(&server.url("/slow-installer")).expect("test URL"),
            &destination,
            &AtomicBool::new(false),
        )
        .await
        .expect_err("timed-out download must fail");

        request.assert_async().await;
        assert!(matches!(error, DownloadError::Timeout));
        assert!(!destination.exists());
    }
}
