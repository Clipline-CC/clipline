//! Direct-to-S3 multipart upload path.
use super::*;

fn validate_object_upload_url(base: &reqwest::Url, raw: &str) -> Result<reqwest::Url, DirectPutError> {
    let url = reqwest::Url::parse(raw).map_err(|_| DirectPutError::Fallback("invalid object upload URL".into()))?;
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some()
        || !matches!(url.scheme(), "https" | "http")
        || (url.origin() != base.origin() && url.scheme() != "https")
    {
        return Err(DirectPutError::Fallback("object URL violates cloud transport policy".into()));
    }
    Ok(url)
}

fn public_object_address(address: std::net::IpAddr) -> bool {
    match address {
        std::net::IpAddr::V4(ip) => {
            let [a, b, _, _] = ip.octets();
            !ip.is_private() && !ip.is_loopback() && !ip.is_link_local()
                && !ip.is_unspecified() && !ip.is_multicast() && !ip.is_broadcast()
                && !ip.is_documentation() && a != 0 && a < 224
                && !(a == 100 && (64..=127).contains(&b))
                && !(a == 198 && (18..=19).contains(&b))
        }
        std::net::IpAddr::V6(ip) => ip.to_ipv4().map(|ip| public_object_address(ip.into())).unwrap_or_else(|| {
            let segments = ip.segments();
            (segments[0] & 0xe000) == 0x2000
                && !(segments[0] == 0x2001 && (segments[1] < 0x0200 || segments[1] == 0x0db8))
                && segments[0] != 0x2002 && segments[0] != 0x3fff
        }),
    }
}

#[cfg(test)]
#[test]
fn security_scan_object_transport_preserves_consent_and_rejects_private_destinations() {
    let base: reqwest::Url = "https://cloud.example/".parse().unwrap();
    assert!(validate_object_upload_url(&base, "https://objects.example/part?signature=secret").is_ok());
    for raw in ["http://objects.example/part", "http://127.0.0.1:80/", "https://user:secret@objects.example/", "file:///part"] {
        assert!(validate_object_upload_url(&base, raw).is_err());
    }
    let local: reqwest::Url = "http://127.0.0.1:8080/".parse().unwrap();
    assert!(validate_object_upload_url(&local, "http://127.0.0.1:8080/part").is_ok());
    assert!(validate_object_upload_url(&local, "http://127.0.0.1:8081/part").is_err());
    for address in ["127.0.0.1", "10.0.0.1", "192.168.0.1", "169.254.169.254", "100.64.0.1", "::1", "::ffff:127.0.0.1", "fc00::1", "fe80::1", "2001:db8::1"] {
        assert!(!public_object_address(address.parse().unwrap()), "{address}");
    }
    for address in ["8.8.8.8", "2606:4700:4700::1111"] {
        assert!(public_object_address(address.parse().unwrap()));
    }
}

pub(crate) async fn upload_chunked_direct<F>(
    transport: UploadTransport<'_>,
    upload: &CreateUploadResponse,
    path: &Path,
    progress: UploadProgressResponse,
    templates: DirectPartTemplates<'_>,
    on_progress: &mut F,
) -> Result<UploadProgressResponse, DirectUploadError>
where
    F: FnMut(&UploadProgressResponse),
{
    let file_size = tokio::fs::metadata(path)
        .await
        .map_err(|error| upload_file_error("read upload metadata", path, error))
        .map_err(DirectUploadError::Cloud)?
        .len();
    validate_missing_parts(&progress.missing_parts, file_size, upload.part_size_bytes)
        .map_err(DirectUploadError::Cloud)?;
    for part_number in progress.missing_parts {
        let part = prepare_upload_part(path, file_size, upload.part_size_bytes, part_number)
            .await
            .map_err(DirectUploadError::Cloud)?;
        upload_direct_part(transport, upload, part_number, path, &part, templates).await?;
        let progress = get_upload_progress(
            transport.client,
            transport.authenticated_control,
            transport.device_token,
            &upload.upload_id,
        )
        .await
        .map_err(DirectUploadError::Cloud)?;
        on_progress(&progress);
    }
    let progress = complete_upload(
        transport.client,
        transport.authenticated_control,
        transport.device_token,
        &upload.upload_id,
    )
    .await
    .map_err(DirectUploadError::Cloud)?;
    on_progress(&progress);
    Ok(progress)
}

pub(crate) async fn upload_direct_part(
    transport: UploadTransport<'_>,
    upload: &CreateUploadResponse,
    part_number: u16,
    path: &Path,
    part: &PreparedUploadPart,
    templates: DirectPartTemplates<'_>,
) -> Result<PartUploadResponse, DirectUploadError> {
    let mut last_retryable_error = None;
    for attempt in 1..=DIRECT_PUT_MAX_ATTEMPTS {
        let presign = request_direct_presign(
            transport.client,
            transport.authenticated_control,
            transport.device_token,
            templates.presign,
            part_number,
        )
        .await?;
        validate_presign(upload, part_number, part.slice.length, &presign)?;

        match put_presigned_part(transport.client, transport.object_http, &presign, path, part.slice).await {
            Ok(etag) => {
                let ack = DirectPartUploadAckRequest {
                    size_bytes: part.slice.length,
                    checksum_sha256: part.checksum_sha256.clone(),
                    etag,
                };
                return ack_direct_part(
                    transport.client,
                    transport.authenticated_control,
                    transport.device_token,
                    templates.ack,
                    part_number,
                    &ack,
                )
                .await;
            }
            Err(DirectPutError::Retryable {
                message,
                retry_after,
            }) => {
                last_retryable_error = Some(message);
                if attempt < DIRECT_PUT_MAX_ATTEMPTS {
                    let delay = direct_put_retry_delay(
                        &upload.upload_id,
                        part_number,
                        attempt,
                        retry_after,
                    );
                    // Tokio's timer is cancellation-safe: aborting or dropping
                    // the upload future cancels this wait immediately.
                    tokio::time::sleep(delay).await;
                }
            }
            Err(DirectPutError::Fallback(message)) => {
                return Err(DirectUploadError::Fallback(message));
            }
            Err(DirectPutError::Terminal(error)) => {
                return Err(DirectUploadError::Cloud(error));
            }
        }
    }

    Err(DirectUploadError::Cloud(CloudApiError::InvalidUpload(
        format!(
            "direct S3 PUT for part {part_number} failed after refreshing presign: {}",
            last_retryable_error.unwrap_or_else(|| "unknown error".to_string())
        ),
    )))
}

pub(crate) async fn request_direct_presign(
    client: &CloudClient,
    http: &reqwest::Client,
    device_token: &str,
    template: &str,
    part_number: u16,
) -> Result<DirectPartUploadUrlResponse, DirectUploadError> {
    let url = upload_url(client, template, part_number).map_err(DirectUploadError::Cloud)?;
    post_empty_with_auth(http, url, device_token)
        .await
        .map_err(classify_direct_control_error)
}

pub(crate) async fn ack_direct_part(
    client: &CloudClient,
    http: &reqwest::Client,
    device_token: &str,
    template: &str,
    part_number: u16,
    ack: &DirectPartUploadAckRequest,
) -> Result<PartUploadResponse, DirectUploadError> {
    let url = upload_url(client, template, part_number).map_err(DirectUploadError::Cloud)?;
    post_json_with_auth(http, url, device_token, ack)
        .await
        .map_err(classify_direct_control_error)
}

pub(crate) fn validate_presign(
    upload: &CreateUploadResponse,
    part_number: u16,
    part_length: u64,
    presign: &DirectPartUploadUrlResponse,
) -> Result<(), DirectUploadError> {
    if presign.upload_id != upload.upload_id || presign.part_number != part_number {
        return Err(DirectUploadError::Fallback(
            "direct S3 presign response did not match the requested part".to_string(),
        ));
    }
    if !presign.method.eq_ignore_ascii_case("PUT") {
        return Err(DirectUploadError::Fallback(format!(
            "direct S3 presign returned unsupported method {:?}",
            presign.method
        )));
    }
    if presign.expected_size_bytes != part_length {
        return Err(DirectUploadError::Fallback(format!(
            "direct S3 presign expected {} bytes for part {part_number}, but the client has {}",
            presign.expected_size_bytes, part_length
        )));
    }
    Ok(())
}

pub(crate) async fn put_presigned_part(
    client: &CloudClient,
    http: &reqwest::Client,
    presign: &DirectPartUploadUrlResponse,
    path: &Path,
    slice: FileSlice,
) -> Result<String, DirectPutError> {
    let url = validate_object_upload_url(client.base_url(), &presign.url)?;
    let pinned_http;
    let http = if url.origin() == client.base_url().origin() {
        http
    } else {
        let host = url.host_str().ok_or_else(|| DirectPutError::Fallback("object URL has no host".into()))?;
        let addresses = tokio::time::timeout(Duration::from_secs(5),
            tokio::net::lookup_host((host, url.port_or_known_default().unwrap_or(443))))
            .await.map_err(|_| DirectPutError::Fallback("object host resolution timed out".into()))?
            .map_err(|_| DirectPutError::Fallback("object host resolution failed".into()))?
            .collect::<Vec<_>>();
        if addresses.is_empty() || addresses.iter().any(|addr| !public_object_address(addr.ip())) {
            return Err(DirectPutError::Fallback("object URL must resolve to public addresses".into()));
        }
        // Pin the validated DNS answer and bypass proxies to prevent rebinding into the LAN.
        pinned_http = crate::bounded_http::object_stream_client_builder()
            .resolve_to_addrs(host, &addresses).build()
            .map_err(|_| DirectPutError::Fallback("could not create object transport".into()))?;
        &pinned_http
    };
    let body = part_request_body(path, slice)
        .await
        .map_err(DirectPutError::Terminal)?;
    let mut request = http
        .put(url)
        .header(header::CONTENT_LENGTH, slice.length)
        .body(body);
    for header in &presign.headers {
        let name = header::HeaderName::from_bytes(header.name.as_bytes()).map_err(|e| {
            DirectPutError::Fallback(format!(
                "direct S3 presign returned invalid header name {:?}: {e}",
                header.name
            ))
        })?;
        let value = header::HeaderValue::from_str(&header.value).map_err(|e| {
            DirectPutError::Fallback(format!(
                "direct S3 presign returned invalid header value for {:?}: {e}",
                header.name
            ))
        })?;
        request = request.header(name, value);
    }

    let response = request
        .timeout(crate::bounded_http::upload_timeout(slice.length))
        .send()
        .await
        .map_err(classify_direct_put_transport_error)?;
    let status = response.status();
    if !status.is_success() {
        let message = format!("direct S3 PUT failed with {status}");
        if is_retryable_direct_put_status(status) {
            let retry_after = response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| parse_retry_after(value, SystemTime::now()));
            return Err(DirectPutError::Retryable {
                message,
                retry_after,
            });
        }
        return Err(DirectPutError::Fallback(message));
    }

    let etag = response
        .headers()
        .get(header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            DirectPutError::Terminal(CloudApiError::InvalidUpload(
                "direct S3 upload did not return an ETag for the uploaded part".to_string(),
            ))
        })?;
    Ok(etag)
}
