//! Shared test helpers for the cloud_upload modules.
use super::*;

use clipline_cloud_api::sha256_hex;
use httpmock::prelude::*;
use serde_json::json;
use httpmock::Mock;

#[cfg(test)]
pub(crate) async fn upload_mp4_bytes_with_progress<F>(
    client: &CloudClient,
    device_token: &str,
    request: &CreateUploadRequest,
    description: Option<&str>,
    bytes: &[u8],
    mut on_progress: F,
) -> CloudApiResult<UploadProgressResponse>
where
    F: FnMut(&UploadProgressResponse),
{
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_UPLOAD_COUNTER: AtomicU64 = AtomicU64::new(0);
    let suffix = TEST_UPLOAD_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "clipline-upload-test-{}-{suffix}.mp4",
        std::process::id()
    ));
    tokio::fs::write(&path, bytes)
        .await
        .map_err(|error| upload_file_error("write test upload", &path, error))?;
    let result = upload_mp4_file_with_progress(
        client,
        device_token,
        request,
        description,
        &path,
        |progress| on_progress(progress),
    )
    .await;
    let _ = tokio::fs::remove_file(path).await;
    result
}


pub(crate) const TOKEN: &str = "device-token";

pub(crate) fn test_client(server: &MockServer) -> CloudClient {
    let base_url =
        clipline_cloud_api::validate_cloud_host(&server.base_url(), true).expect("cloud URL");
    CloudClient::with_device_token(base_url, TOKEN)
}

pub(crate) fn upload_request(bytes: &[u8]) -> CreateUploadRequest {
    CreateUploadRequest {
        client_clip_id: Some("local-1".to_string()),
        title: "clip".to_string(),
        description: None,
        game_name: None,
        game_id: None,
        game_executable: None,
        source_type: Some("replay".to_string()),
        recorded_at: None,
        duration_ms: None,
        file_size_bytes: bytes.len() as u64,
        checksum_sha256: sha256_hex(bytes),
        container: "mp4".to_string(),
        video_codec: Some("h264".to_string()),
        audio_codec: None,
        width: None,
        height: None,
        fps: None,
        visibility: Some("private".to_string()),
        markers: None,
    }
}

pub(crate) fn mount_discovery(server: &MockServer, direct_s3_upload: bool) -> Mock<'_> {
    server.mock(|when, then| {
        when.method(GET).path("/.well-known/clipline-cloud");
        then.status(200).json_body(json!({
            "name": "Clipline Cloud",
            "api_version": "v1",
            "server_version": "1.0.0",
            "min_client_version": "0.1.0",
            "public_url": server.base_url(),
            "features": {
                "single_put_upload": true,
                "chunked_upload": true,
                "direct_s3_upload": direct_s3_upload,
                "public_sharing": true,
                "clip_markers": true,
                "max_upload_size_bytes": 1000000
            }
        }));
    })
}

pub(crate) fn mount_chunked_create<'a>(
    server: &'a MockServer,
    upload_id: &str,
    clip_id: &str,
    presign_template: Option<&str>,
    ack_template: Option<&str>,
) -> Mock<'a> {
    server.mock(|when, then| {
        when.method(POST).path("/api/v1/uploads");
        then.status(200).json_body(json!({
            "clip_id": clip_id,
            "upload_id": upload_id,
            "mode": "chunked",
            "part_size_bytes": 3,
            "single_put_url": null,
            "parts_url_template": format!("/api/v1/uploads/{upload_id}/parts/{{part_number}}"),
            "direct_part_presign_url_template": presign_template,
            "direct_part_ack_url_template": ack_template
        }));
    })
}

pub(crate) fn mount_progress<'a>(
    server: &'a MockServer,
    upload_id: &str,
    clip_id: &str,
    status: &str,
    file_size: u64,
    missing_parts: Vec<u16>,
) -> Mock<'a> {
    server.mock(|when, then| {
        when.method(GET)
            .path(format!("/api/v1/uploads/{upload_id}"));
        then.status(200).json_body(progress_json(
            upload_id,
            clip_id,
            "chunked",
            status,
            file_size,
            0,
            missing_parts,
        ));
    })
}

pub(crate) fn mount_proxy_part<'a>(
    server: &'a MockServer,
    upload_id: &str,
    part_number: u16,
    body: &str,
) -> Mock<'a> {
    server.mock(|when, then| {
        when.method(PUT)
            .path(format!("/api/v1/uploads/{upload_id}/parts/{part_number}"))
            .header("content-length", body.len().to_string())
            .header("x-clipline-part-sha256", sha256_hex(body.as_bytes()))
            .body(body);
        then.status(200).json_body(json!({
            "upload_id": upload_id,
            "part_number": part_number,
            "size_bytes": body.len(),
            "checksum_sha256": sha256_hex(body.as_bytes()),
            "etag": null,
            "idempotent": false
        }));
    })
}

pub(crate) fn mount_presign<'a>(
    cloud: &'a MockServer,
    s3: &MockServer,
    upload_id: &str,
    part_number: u16,
    expected_size_bytes: u64,
    s3_path: &str,
    expected_body: &str,
) -> Mock<'a> {
    cloud.mock(|when, then| {
        when.method(POST).path(format!(
            "/api/v1/uploads/{upload_id}/parts/{part_number}/direct-presign"
        ));
        then.status(200).json_body(json!({
            "upload_id": upload_id,
            "part_number": part_number,
            "method": "PUT",
            "url": format!("{}{}", s3.base_url(), s3_path),
            "expires_at": "2030-01-01T00:00:00Z",
            "expected_size_bytes": expected_size_bytes,
            "headers": [
                { "name": "x-amz-meta-clipline-test", "value": expected_body }
            ]
        }));
    })
}

pub(crate) fn mount_s3_put<'a>(
    s3: &'a MockServer,
    path: &str,
    body: &str,
    etag: &str,
    status: u16,
) -> Mock<'a> {
    s3.mock(|when, then| {
        when.method(PUT)
            .path(path)
            .header("content-length", body.len().to_string())
            .header("x-amz-meta-clipline-test", body)
            .body(body);
        then.status(status).header("ETag", etag);
    })
}

pub(crate) fn mount_ack<'a>(
    server: &'a MockServer,
    upload_id: &str,
    part_number: u16,
    etag: &str,
    body: &str,
    status: u16,
) -> Mock<'a> {
    server.mock(|when, then| {
        when.method(POST)
            .path(format!(
                "/api/v1/uploads/{upload_id}/parts/{part_number}/direct-ack"
            ))
            .json_body(json!({
                "size_bytes": body.len(),
                "checksum_sha256": sha256_hex(body.as_bytes()),
                "etag": etag
            }));
        if status == 409 {
            then.status(409)
                .json_body(json!({ "error": "part metadata conflict" }));
        } else {
            then.status(status).json_body(json!({
                "upload_id": upload_id,
                "part_number": part_number,
                "size_bytes": body.len(),
                "checksum_sha256": sha256_hex(body.as_bytes()),
                "etag": etag,
                "idempotent": false
            }));
        }
    })
}

pub(crate) fn mount_complete<'a>(
    server: &'a MockServer,
    upload_id: &str,
    clip_id: &str,
    file_size: u64,
) -> Mock<'a> {
    server.mock(|when, then| {
        when.method(POST)
            .path(format!("/api/v1/uploads/{upload_id}/complete"));
        then.status(200).json_body(progress_json(
            upload_id,
            clip_id,
            "chunked",
            "completed",
            file_size,
            file_size,
            vec![],
        ));
    })
}

pub(crate) fn progress_json(
    upload_id: &str,
    clip_id: &str,
    mode: &str,
    status: &str,
    file_size: u64,
    received_size: u64,
    missing_parts: Vec<u16>,
) -> serde_json::Value {
    let missing_part_count = missing_parts.len() as u16;
    let progress_basis_points = received_size
        .saturating_mul(10000)
        .checked_div(file_size)
        .unwrap_or(0) as u16;
    json!({
        "upload_id": upload_id,
        "clip_id": clip_id,
        "mode": mode,
        "status": status,
        "file_size_bytes": file_size,
        "part_size_bytes": 3,
        "received_size_bytes": received_size,
        "total_parts": 2,
        "received_part_count": 2_u16.saturating_sub(missing_part_count),
        "missing_part_count": missing_part_count,
        "next_part_number": missing_parts.first().copied(),
        "progress_basis_points": progress_basis_points,
        "failure_reason": null,
        "recovery_action": null,
        "expires_at": "2030-01-01T00:00:00Z",
        "received_parts": [],
        "missing_parts": missing_parts
    })
}
