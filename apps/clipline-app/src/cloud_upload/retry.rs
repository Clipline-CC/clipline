//! Direct/proxy retry classification and backoff.
use super::*;

pub(crate) fn classify_direct_control_error(error: CloudApiError) -> DirectUploadError {
    match error {
        CloudApiError::Api { status, message } if status == StatusCode::CONFLICT => {
            DirectUploadError::Cloud(CloudApiError::Api {
                status,
                message: format!(
                    "direct S3 part acknowledgement conflicted with existing metadata: {message}. Retry the upload from the beginning."
                ),
            })
        }
        CloudApiError::Api { status, message } if is_direct_control_fallback_status(status) => {
            DirectUploadError::Fallback(format!(
                "direct S3 control endpoint is unavailable ({status}): {message}"
            ))
        }
        other => DirectUploadError::Cloud(other),
    }
}

pub(crate) fn is_direct_control_fallback_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 404 | 405 | 410 | 501 | 503)
}

pub(crate) fn is_retryable_direct_put_status(status: StatusCode) -> bool {
    status == StatusCode::FORBIDDEN
        || status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

pub(crate) fn is_retryable_proxy_put_status(status: StatusCode) -> bool {
    status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

pub(crate) fn classify_direct_put_transport_error(error: reqwest::Error) -> DirectPutError {
    let message = format!("direct S3 PUT request failed: {error}");
    if error.is_builder() || error.is_redirect() {
        DirectPutError::Fallback(message)
    } else {
        DirectPutError::Retryable {
            message,
            retry_after: None,
        }
    }
}

pub(crate) fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let timestamp = chrono::DateTime::parse_from_rfc2822(value)
        .ok()?
        .timestamp();
    let timestamp = u64::try_from(timestamp).ok()?;
    let target = UNIX_EPOCH.checked_add(Duration::from_secs(timestamp))?;
    Some(target.duration_since(now).unwrap_or(Duration::ZERO))
}

pub(crate) fn direct_put_retry_delay(
    upload_id: &str,
    part_number: u16,
    failed_attempt: usize,
    retry_after: Option<Duration>,
) -> Duration {
    let exponent = u32::try_from(failed_attempt.saturating_sub(1).min(16)).unwrap_or(16);
    let exponential = DIRECT_PUT_BACKOFF_BASE.saturating_mul(1_u32 << exponent);
    let jitter_window_ms = u64::try_from((exponential / 2).as_millis()).unwrap_or(u64::MAX);
    let mut hasher = DefaultHasher::new();
    upload_id.hash(&mut hasher);
    part_number.hash(&mut hasher);
    failed_attempt.hash(&mut hasher);
    let jitter_ms = hasher.finish() % jitter_window_ms.saturating_add(1);
    let local_delay = exponential.saturating_add(Duration::from_millis(jitter_ms));
    local_delay
        .max(retry_after.unwrap_or(Duration::ZERO))
        .min(DIRECT_PUT_BACKOFF_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_put_retry_delay_is_exponential_jittered_and_bounded() {
        let first = direct_put_retry_delay("upload-1", 7, 1, None);
        let first_again = direct_put_retry_delay("upload-1", 7, 1, None);
        let second = direct_put_retry_delay("upload-1", 7, 2, None);

        assert_eq!(first, first_again, "jitter must be deterministic per part");
        assert!(first >= DIRECT_PUT_BACKOFF_BASE);
        assert!(first < DIRECT_PUT_BACKOFF_BASE * 2);
        assert!(second >= DIRECT_PUT_BACKOFF_BASE * 2);
        assert!(second < DIRECT_PUT_BACKOFF_BASE * 4);
        assert_eq!(
            direct_put_retry_delay("upload-1", 7, 1, Some(Duration::from_secs(3))),
            Duration::from_secs(3)
        );
        assert_eq!(
            direct_put_retry_delay("upload-1", 7, 1, Some(Duration::from_secs(3_600))),
            DIRECT_PUT_BACKOFF_MAX
        );
    }

    #[test]
    fn retry_after_parser_accepts_seconds_and_http_dates() {
        let date = chrono::DateTime::parse_from_rfc2822("Wed, 21 Oct 2015 07:28:00 GMT").unwrap();
        let date_time =
            std::time::UNIX_EPOCH + Duration::from_secs(u64::try_from(date.timestamp()).unwrap());
        let five_seconds_earlier = date_time - Duration::from_secs(5);

        assert_eq!(
            parse_retry_after("7", five_seconds_earlier),
            Some(Duration::from_secs(7))
        );
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT", five_seconds_earlier),
            Some(Duration::from_secs(5))
        );
        assert_eq!(
            parse_retry_after(
                "Wed, 21 Oct 2015 07:28:00 GMT",
                date_time + Duration::from_secs(1)
            ),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after("later", five_seconds_earlier), None);
    }

}