//! Shared HTTP resource limits for desktop integrations.

use std::sync::OnceLock;
use std::time::Duration;

use serde::de::DeserializeOwned;

pub(crate) const CONTROL_JSON_MAX_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const ERROR_BODY_MAX_BYTES: usize = 64 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const CONTROL_READ_TIMEOUT: Duration = Duration::from_secs(15);
const CONTROL_TOTAL_TIMEOUT: Duration = Duration::from_secs(30);
const STREAM_READ_TIMEOUT: Duration = Duration::from_secs(30);
const MIN_UPLOAD_THROUGHPUT_BYTES_PER_SECOND: u64 = 256 * 1024;
const MIN_UPLOAD_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_UPLOAD_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

static CONTROL_CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
static AUTHENTICATED_STREAM_CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();

pub(crate) fn control_client() -> Result<&'static reqwest::Client, String> {
    cached_client(&CONTROL_CLIENT, || {
        reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(CONTROL_READ_TIMEOUT)
            .timeout(CONTROL_TOTAL_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| format!("build bounded HTTP control client: {error}"))
    })
}

pub(crate) fn authenticated_stream_client() -> Result<&'static reqwest::Client, String> {
    cached_client(&AUTHENTICATED_STREAM_CLIENT, || {
        reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(STREAM_READ_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| format!("build bounded authenticated HTTP stream client: {error}"))
    })
}

fn cached_client(
    slot: &'static OnceLock<Result<reqwest::Client, String>>,
    build: impl FnOnce() -> Result<reqwest::Client, String>,
) -> Result<&'static reqwest::Client, String> {
    slot.get_or_init(build).as_ref().map_err(Clone::clone)
}

pub(crate) fn upload_timeout(size_bytes: u64) -> Duration {
    let transfer_seconds = size_bytes.saturating_add(MIN_UPLOAD_THROUGHPUT_BYTES_PER_SECOND - 1)
        / MIN_UPLOAD_THROUGHPUT_BYTES_PER_SECOND;
    MIN_UPLOAD_TIMEOUT
        .saturating_add(Duration::from_secs(transfer_seconds))
        .min(MAX_UPLOAD_TIMEOUT)
}

pub(crate) async fn response_bytes_limited(
    mut response: reqwest::Response,
    max_bytes: usize,
    context: &str,
) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(format!(
            "{context} response is too large (limit {} bytes)",
            max_bytes
        ));
    }

    let mut bytes =
        Vec::with_capacity(response.content_length().unwrap_or(0).min(max_bytes as u64) as usize);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("read {context} response: {error}"))?
    {
        append_limited(&mut bytes, &chunk, max_bytes, context)?;
    }
    Ok(bytes)
}

pub(crate) async fn response_json_limited<T: DeserializeOwned>(
    response: reqwest::Response,
    max_bytes: usize,
    context: &str,
) -> Result<T, String> {
    let bytes = response_bytes_limited(response, max_bytes, context).await?;
    serde_json::from_slice(&bytes).map_err(|error| format!("parse {context} response: {error}"))
}

pub(crate) async fn response_error_message(
    response: reqwest::Response,
    status: reqwest::StatusCode,
    context: &str,
) -> String {
    match response_bytes_limited(response, ERROR_BODY_MAX_BYTES, context).await {
        Ok(bytes) => {
            let message = serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|value| value.get("error")?.as_str().map(str::to_string))
                .unwrap_or_else(|| String::from_utf8_lossy(&bytes).trim().to_string());
            if message.is_empty() {
                status.to_string()
            } else {
                message
            }
        }
        Err(error) => format!("{status} ({error})"),
    }
}

fn append_limited(
    output: &mut Vec<u8>,
    chunk: &[u8],
    max_bytes: usize,
    context: &str,
) -> Result<(), String> {
    let total = output
        .len()
        .checked_add(chunk.len())
        .ok_or_else(|| format!("{context} response size overflow"))?;
    if total > max_bytes {
        return Err(format!(
            "{context} response is too large (limit {} bytes)",
            max_bytes
        ));
    }
    output.extend_from_slice(chunk);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    #[test]
    fn chunk_accumulator_accepts_exact_limit_and_rejects_next_byte() {
        let mut output = Vec::new();
        append_limited(&mut output, b"1234", 5, "fixture").unwrap();
        append_limited(&mut output, b"5", 5, "fixture").unwrap();
        let error = append_limited(&mut output, b"6", 5, "fixture").unwrap_err();

        assert_eq!(output, b"12345");
        assert!(error.contains("too large"));
    }

    #[test]
    fn upload_deadline_scales_with_size_and_stays_bounded() {
        assert_eq!(upload_timeout(0), MIN_UPLOAD_TIMEOUT);
        assert!(upload_timeout(4 * 1024 * 1024) > MIN_UPLOAD_TIMEOUT);
        assert!(upload_timeout(u64::MAX) <= MAX_UPLOAD_TIMEOUT);
    }

    #[tokio::test]
    async fn advertised_oversized_body_is_rejected_before_buffering() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/oversized");
            then.status(200).body("12345");
        });
        let response = control_client()
            .unwrap()
            .get(format!("{}/oversized", server.base_url()))
            .send()
            .await
            .unwrap();

        let error = response_bytes_limited(response, 4, "fixture")
            .await
            .unwrap_err();

        assert!(error.contains("too large"));
    }
}
