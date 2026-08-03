use std::time::Duration;

use clipline_library::ports::CloudCredential;
use clipline_library::protocol::UPLOAD_PART_SHA256_HEADER;
use clipline_library::protocol::{
    sha256_hex, CloudApiBase, CloudProtocolError, CreateMarkerRequest, CreateUploadRequest,
};
use clipline_library::{
    ReqwestUploadTransport, UploadCancellation, MAX_CONCURRENT_UPLOADS, MAX_UPLOAD_PART_BYTES,
    UPLOAD_PUT_MAX_ATTEMPTS,
};
use clipline_test_utils::TestDir;
use httpmock::prelude::{GET, POST, PUT};
use httpmock::MockServer;
use serde_json::json;

const TOKEN: &str = "device-token";
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn api(server: &MockServer) -> CloudApiBase {
    CloudApiBase::parse(&server.base_url(), true).unwrap()
}

fn payload(name: &str, bytes: &[u8]) -> (TestDir, std::path::PathBuf) {
    let directory = TestDir::new("clipline-upload-transport", name);
    let path = directory.path().join("payload.mp4");
    std::fs::write(&path, bytes).unwrap();
    (directory, path)
}

fn request(bytes: &[u8]) -> CreateUploadRequest {
    CreateUploadRequest {
        client_clip_id: Some("local-1".into()),
        title: "clip".into(),
        description: None,
        game_name: None,
        game_id: None,
        game_executable: None,
        source_type: Some("replay".into()),
        recorded_at: None,
        duration_ms: None,
        file_size_bytes: bytes.len() as u64,
        checksum_sha256: sha256_hex(bytes),
        container: "mp4".into(),
        video_codec: Some("h264".into()),
        audio_codec: None,
        width: None,
        height: None,
        fps: None,
        visibility: Some("private".into()),
        markers: None,
    }
}

fn mount_discovery(server: &MockServer, direct: bool) -> httpmock::Mock<'_> {
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
                "direct_s3_upload": direct,
                "public_sharing": true,
                "clip_markers": true,
                "max_upload_size_bytes": 1000000
            }
        }));
    })
}

fn progress_json(
    upload_id: &str,
    clip_id: &str,
    mode: &str,
    status: &str,
    file_size: u64,
    received_size: u64,
    missing_parts: Vec<u16>,
) -> serde_json::Value {
    let missing_count = missing_parts.len() as u16;
    json!({
        "upload_id": upload_id,
        "clip_id": clip_id,
        "mode": mode,
        "status": status,
        "file_size_bytes": file_size,
        "part_size_bytes": 3,
        "received_size_bytes": received_size,
        "total_parts": 2,
        "received_part_count": 2_u16.saturating_sub(missing_count),
        "missing_part_count": missing_count,
        "next_part_number": missing_parts.first().copied(),
        "progress_basis_points": if file_size == 0 { 0 } else {
            received_size.saturating_mul(10_000).checked_div(file_size).unwrap_or(0)
        },
        "failure_reason": null,
        "recovery_action": null,
        "expires_at": "2030-01-01T00:00:00Z",
        "received_parts": [],
        "missing_parts": missing_parts
    })
}

fn mount_single_create<'a>(
    server: &'a MockServer,
    upload_id: &str,
    clip_id: &str,
    size: usize,
) -> httpmock::Mock<'a> {
    server.mock(|when, then| {
        when.method(POST).path("/api/v1/uploads");
        then.status(200).json_body(json!({
            "clip_id": clip_id,
            "upload_id": upload_id,
            "mode": "single_put",
            "part_size_bytes": size,
            "single_put_url": format!("/api/v1/uploads/{upload_id}/content"),
            "parts_url_template": null
        }));
    })
}

fn mount_progress<'a>(
    server: &'a MockServer,
    upload_id: &str,
    clip_id: &str,
    mode: &str,
    status: &str,
    size: usize,
    missing: Vec<u16>,
) -> httpmock::Mock<'a> {
    server.mock(|when, then| {
        when.method(GET)
            .path(format!("/api/v1/uploads/{upload_id}"))
            .header("authorization", format!("Bearer {TOKEN}"));
        then.status(200).json_body(progress_json(
            upload_id,
            clip_id,
            mode,
            status,
            size as u64,
            0,
            missing,
        ));
    })
}

#[tokio::test]
async fn single_put_preserves_bytes_content_type_and_create_body_shape() {
    let _serial = TEST_LOCK.lock().await;
    let bytes = b"single body";
    let (_directory, path) = payload("single", bytes);
    let server = MockServer::start();
    mount_discovery(&server, true);
    let create = server.mock(|when, then| {
        when.method(POST)
            .path("/api/v1/uploads")
            .header("authorization", format!("Bearer {TOKEN}"))
            .json_body_partial(r#"{"description":"Useful context"}"#);
        then.status(200).json_body(json!({
            "clip_id": "c1", "upload_id": "u1", "mode": "single_put",
            "part_size_bytes": bytes.len(),
            "single_put_url": "/api/v1/uploads/u1/content",
            "parts_url_template": null
        }));
    });
    mount_progress(
        &server,
        "u1",
        "c1",
        "single_put",
        "uploading",
        bytes.len(),
        vec![],
    );
    let put = server.mock(|when, then| {
        when.method(PUT)
            .path("/api/v1/uploads/u1/content")
            .header("authorization", format!("Bearer {TOKEN}"))
            .header("content-type", "video/mp4")
            .header("content-length", bytes.len().to_string())
            .body("single body");
        then.status(200).json_body(progress_json(
            "u1",
            "c1",
            "single_put",
            "completed",
            bytes.len() as u64,
            bytes.len() as u64,
            vec![],
        ));
    });
    let mut upload_request = request(bytes);
    upload_request.markers = Some(vec![CreateMarkerRequest {
        kind: "ChampionKill".into(),
        label: Some("kill".into()),
        timestamp_ms: 1_200,
        metadata: Some(json!({"deprecated": true})),
    }]);

    let result = ReqwestUploadTransport::new()
        .unwrap()
        .upload_file_with_progress(
            &api(&server),
            &CloudCredential::new(TOKEN),
            &upload_request,
            Some("  Useful context  "),
            &path,
            &UploadCancellation::default(),
            |_| {},
        )
        .await
        .unwrap();

    assert_eq!(result.status, "completed");
    create.assert();
    put.assert();
}

#[tokio::test]
async fn request_metadata_mismatch_fails_before_network_io() {
    let _serial = TEST_LOCK.lock().await;
    let bytes = b"payload";
    let (_directory, path) = payload("metadata-mismatch", bytes);
    let server = MockServer::start();
    let discovery = mount_discovery(&server, false);
    let mut bad_size = request(bytes);
    bad_size.file_size_bytes += 1;
    let transport = ReqwestUploadTransport::new().unwrap();
    let credential = CloudCredential::new(TOKEN);

    let error = transport
        .upload_file_with_progress(
            &api(&server),
            &credential,
            &bad_size,
            None,
            &path,
            &UploadCancellation::default(),
            |_| {},
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("file_size_bytes"));
    discovery.assert_hits(0);

    let mut bad_hash = request(bytes);
    bad_hash.checksum_sha256 = "00".repeat(32);
    let error = transport
        .upload_file_with_progress(
            &api(&server),
            &credential,
            &bad_hash,
            None,
            &path,
            &UploadCancellation::default(),
            |_| {},
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("checksum_sha256"));
    discovery.assert_hits(0);
}

#[tokio::test]
async fn proxy_multipart_streams_resumable_parts_and_completes() {
    let _serial = TEST_LOCK.lock().await;
    let bytes = b"abcdef";
    let (_directory, path) = payload("proxy", bytes);
    let server = MockServer::start();
    mount_discovery(&server, false);
    server.mock(|when, then| {
        when.method(POST).path("/api/v1/uploads");
        then.status(200).json_body(json!({
            "clip_id": "c1", "upload_id": "u1", "mode": "chunked",
            "part_size_bytes": 3, "single_put_url": null,
            "parts_url_template": "/api/v1/uploads/u1/parts/{part_number}"
        }));
    });
    mount_progress(
        &server,
        "u1",
        "c1",
        "chunked",
        "uploading",
        bytes.len(),
        vec![2, 1],
    );
    let second = server.mock(|when, then| {
        when.method(PUT)
            .path("/api/v1/uploads/u1/parts/2")
            .header("content-type", "video/mp4")
            .header(UPLOAD_PART_SHA256_HEADER, sha256_hex(b"def"))
            .body("def");
        then.status(200).json_body(json!({
            "upload_id": "u1", "part_number": 2, "size_bytes": 3,
            "checksum_sha256": sha256_hex(b"def"), "etag": null, "idempotent": false
        }));
    });
    let first = server.mock(|when, then| {
        when.method(PUT)
            .path("/api/v1/uploads/u1/parts/1")
            .header("content-type", "video/mp4")
            .header(UPLOAD_PART_SHA256_HEADER, sha256_hex(b"abc"))
            .body("abc");
        then.status(200).json_body(json!({
            "upload_id": "u1", "part_number": 1, "size_bytes": 3,
            "checksum_sha256": sha256_hex(b"abc"), "etag": null, "idempotent": false
        }));
    });
    let complete = server.mock(|when, then| {
        when.method(POST).path("/api/v1/uploads/u1/complete");
        then.status(200).json_body(progress_json(
            "u1",
            "c1",
            "chunked",
            "completed",
            6,
            6,
            vec![],
        ));
    });

    let result = ReqwestUploadTransport::new()
        .unwrap()
        .upload_file_with_progress(
            &api(&server),
            &CloudCredential::new(TOKEN),
            &request(bytes),
            None,
            &path,
            &UploadCancellation::default(),
            |_| {},
        )
        .await
        .unwrap();

    assert_eq!(result.status, "completed");
    second.assert();
    first.assert();
    complete.assert();
}

#[tokio::test]
async fn proxy_part_retries_exactly_three_times_with_fresh_streams() {
    let _serial = TEST_LOCK.lock().await;
    let bytes = b"abc";
    let (_directory, path) = payload("proxy-retry", bytes);
    let server = MockServer::start();
    mount_discovery(&server, false);
    server.mock(|when, then| {
        when.method(POST).path("/api/v1/uploads");
        then.status(200).json_body(json!({
            "clip_id": "c1", "upload_id": "u1", "mode": "chunked",
            "part_size_bytes": 3, "single_put_url": null,
            "parts_url_template": null
        }));
    });
    mount_progress(
        &server,
        "u1",
        "c1",
        "chunked",
        "uploading",
        bytes.len(),
        vec![1],
    );
    let failed = server.mock(|when, then| {
        when.method(PUT)
            .path("/api/v1/uploads/u1/parts/1")
            .body("abc");
        then.status(503)
            .json_body(json!({"error": "temporarily unavailable"}));
    });

    let error = ReqwestUploadTransport::new()
        .unwrap()
        .upload_file_with_progress(
            &api(&server),
            &CloudCredential::new(TOKEN),
            &request(bytes),
            None,
            &path,
            &UploadCancellation::default(),
            |_| {},
        )
        .await
        .unwrap_err();

    assert!(error.to_string().contains("temporarily unavailable"));
    failed.assert_hits(UPLOAD_PUT_MAX_ATTEMPTS);
}

#[tokio::test]
async fn direct_s3_presigns_puts_without_bearer_acks_and_completes() {
    let _serial = TEST_LOCK.lock().await;
    let bytes = b"abc";
    let (_directory, path) = payload("direct", bytes);
    let cloud = MockServer::start();
    let object = MockServer::start();
    mount_discovery(&cloud, true);
    cloud.mock(|when, then| {
        when.method(POST).path("/api/v1/uploads");
        then.status(200).json_body(json!({
            "clip_id": "c1", "upload_id": "u1", "mode": "chunked",
            "part_size_bytes": 3, "single_put_url": null,
            "parts_url_template": "/api/v1/uploads/u1/parts/{part_number}",
            "direct_part_presign_url_template": "/api/v1/uploads/u1/parts/{part_number}/direct-presign",
            "direct_part_ack_url_template": "/api/v1/uploads/u1/parts/{part_number}/direct-ack"
        }));
    });
    mount_progress(
        &cloud,
        "u1",
        "c1",
        "chunked",
        "uploading",
        bytes.len(),
        vec![1],
    );
    let presign = cloud.mock(|when, then| {
        when.method(POST)
            .path("/api/v1/uploads/u1/parts/1/direct-presign")
            .header("authorization", format!("Bearer {TOKEN}"));
        then.status(200).json_body(json!({
            "upload_id": "u1", "part_number": 1, "method": "PUT",
            "url": format!("{}/object-part", object.base_url()),
            "expires_at": "2030-01-01T00:00:00Z", "expected_size_bytes": 3,
            "headers": [{"name": "x-amz-meta-test", "value": "abc"}]
        }));
    });
    let put = object.mock(|when, then| {
        when.method(PUT)
            .path("/object-part")
            .header("x-amz-meta-test", "abc")
            .body("abc");
        then.status(200).header("ETag", "\"etag-1\"");
    });
    let ack = cloud.mock(|when, then| {
        when.method(POST)
            .path("/api/v1/uploads/u1/parts/1/direct-ack")
            .json_body(json!({
                "size_bytes": 3,
                "checksum_sha256": sha256_hex(bytes),
                "etag": "\"etag-1\""
            }));
        then.status(200).json_body(json!({
            "upload_id": "u1", "part_number": 1, "size_bytes": 3,
            "checksum_sha256": sha256_hex(bytes), "etag": "\"etag-1\"",
            "idempotent": false
        }));
    });
    let complete = cloud.mock(|when, then| {
        when.method(POST).path("/api/v1/uploads/u1/complete");
        then.status(200).json_body(progress_json(
            "u1",
            "c1",
            "chunked",
            "completed",
            3,
            3,
            vec![],
        ));
    });

    let result = ReqwestUploadTransport::new()
        .unwrap()
        .upload_file_with_progress(
            &api(&cloud),
            &CloudCredential::new(TOKEN),
            &request(bytes),
            None,
            &path,
            &UploadCancellation::default(),
            |_| {},
        )
        .await
        .unwrap();

    assert_eq!(result.status, "completed");
    for item in [presign, put, ack, complete] {
        item.assert();
    }
}

#[tokio::test]
async fn direct_provider_failure_recreates_upload_and_falls_back_to_proxy() {
    let _serial = TEST_LOCK.lock().await;
    let bytes = b"abc";
    let (_directory, path) = payload("direct-fallback", bytes);
    let cloud = MockServer::start();
    let object = MockServer::start();
    mount_discovery(&cloud, true);
    let create = cloud.mock(|when, then| {
        when.method(POST).path("/api/v1/uploads");
        then.status(200).json_body(json!({
            "clip_id": "c1", "upload_id": "u1", "mode": "chunked",
            "part_size_bytes": 3, "single_put_url": null,
            "parts_url_template": "/api/v1/uploads/u1/parts/{part_number}",
            "direct_part_presign_url_template": "/api/v1/uploads/u1/parts/{part_number}/direct-presign",
            "direct_part_ack_url_template": "/api/v1/uploads/u1/parts/{part_number}/direct-ack"
        }));
    });
    mount_progress(
        &cloud,
        "u1",
        "c1",
        "chunked",
        "uploading",
        bytes.len(),
        vec![1],
    );
    cloud.mock(|when, then| {
        when.method(POST)
            .path("/api/v1/uploads/u1/parts/1/direct-presign");
        then.status(200).json_body(json!({
            "upload_id": "u1", "part_number": 1, "method": "PUT",
            "url": format!("{}/provider-failure", object.base_url()),
            "expires_at": "2030-01-01T00:00:00Z", "expected_size_bytes": 3,
            "headers": []
        }));
    });
    let provider_failure = object.mock(|when, then| {
        when.method(PUT).path("/provider-failure");
        then.status(400);
    });
    let proxy = cloud.mock(|when, then| {
        when.method(PUT)
            .path("/api/v1/uploads/u1/parts/1")
            .body("abc");
        then.status(200).json_body(json!({
            "upload_id": "u1", "part_number": 1, "size_bytes": 3,
            "checksum_sha256": sha256_hex(bytes), "etag": null, "idempotent": false
        }));
    });
    cloud.mock(|when, then| {
        when.method(POST).path("/api/v1/uploads/u1/complete");
        then.status(200).json_body(progress_json(
            "u1",
            "c1",
            "chunked",
            "completed",
            3,
            3,
            vec![],
        ));
    });

    ReqwestUploadTransport::new()
        .unwrap()
        .upload_file_with_progress(
            &api(&cloud),
            &CloudCredential::new(TOKEN),
            &request(bytes),
            None,
            &path,
            &UploadCancellation::default(),
            |_| {},
        )
        .await
        .unwrap();

    create.assert_hits(2);
    provider_failure.assert();
    proxy.assert();
}

#[tokio::test]
async fn direct_expiry_retries_with_a_fresh_presign_exactly_three_times() {
    let _serial = TEST_LOCK.lock().await;
    let bytes = b"abc";
    let (_directory, path) = payload("direct-retry", bytes);
    let cloud = MockServer::start();
    let object = MockServer::start();
    mount_discovery(&cloud, true);
    cloud.mock(|when, then| {
        when.method(POST).path("/api/v1/uploads");
        then.status(200).json_body(json!({
            "clip_id": "c1", "upload_id": "u1", "mode": "chunked",
            "part_size_bytes": 3, "single_put_url": null,
            "parts_url_template": null,
            "direct_part_presign_url_template": "/api/v1/uploads/u1/parts/{part_number}/direct-presign",
            "direct_part_ack_url_template": "/api/v1/uploads/u1/parts/{part_number}/direct-ack"
        }));
    });
    mount_progress(
        &cloud,
        "u1",
        "c1",
        "chunked",
        "uploading",
        bytes.len(),
        vec![1],
    );
    let presign = cloud.mock(|when, then| {
        when.method(POST)
            .path("/api/v1/uploads/u1/parts/1/direct-presign");
        then.status(200).json_body(json!({
            "upload_id": "u1", "part_number": 1, "method": "PUT",
            "url": format!("{}/expired", object.base_url()),
            "expires_at": "2030-01-01T00:00:00Z", "expected_size_bytes": 3,
            "headers": []
        }));
    });
    let expired = object.mock(|when, then| {
        when.method(PUT).path("/expired").body("abc");
        then.status(403);
    });

    let error = ReqwestUploadTransport::new()
        .unwrap()
        .upload_file_with_progress(
            &api(&cloud),
            &CloudCredential::new(TOKEN),
            &request(bytes),
            None,
            &path,
            &UploadCancellation::default(),
            |_| {},
        )
        .await
        .unwrap_err();

    assert!(error.to_string().contains("refreshing presign"));
    presign.assert_hits(UPLOAD_PUT_MAX_ATTEMPTS);
    expired.assert_hits(UPLOAD_PUT_MAX_ATTEMPTS);
}

#[tokio::test]
async fn direct_success_without_etag_is_terminal_and_does_not_proxy_fallback() {
    let _serial = TEST_LOCK.lock().await;
    let bytes = b"abc";
    let (_directory, path) = payload("direct-etag", bytes);
    let cloud = MockServer::start();
    let object = MockServer::start();
    mount_discovery(&cloud, true);
    let create = cloud.mock(|when, then| {
        when.method(POST).path("/api/v1/uploads");
        then.status(200).json_body(json!({
            "clip_id": "c1", "upload_id": "u1", "mode": "chunked",
            "part_size_bytes": 3, "single_put_url": null,
            "parts_url_template": null,
            "direct_part_presign_url_template": "/api/v1/uploads/u1/parts/{part_number}/direct-presign",
            "direct_part_ack_url_template": "/api/v1/uploads/u1/parts/{part_number}/direct-ack"
        }));
    });
    mount_progress(
        &cloud,
        "u1",
        "c1",
        "chunked",
        "uploading",
        bytes.len(),
        vec![1],
    );
    cloud.mock(|when, then| {
        when.method(POST)
            .path("/api/v1/uploads/u1/parts/1/direct-presign");
        then.status(200).json_body(json!({
            "upload_id": "u1", "part_number": 1, "method": "PUT",
            "url": format!("{}/missing-etag", object.base_url()),
            "expires_at": "2030-01-01T00:00:00Z", "expected_size_bytes": 3,
            "headers": []
        }));
    });
    object.mock(|when, then| {
        when.method(PUT).path("/missing-etag");
        then.status(200);
    });

    let error = ReqwestUploadTransport::new()
        .unwrap()
        .upload_file_with_progress(
            &api(&cloud),
            &CloudCredential::new(TOKEN),
            &request(bytes),
            None,
            &path,
            &UploadCancellation::default(),
            |_| {},
        )
        .await
        .unwrap_err();

    assert!(error.to_string().contains("did not return an ETag"));
    create.assert_hits(1);
}

#[tokio::test]
async fn direct_ack_conflict_returns_retry_guidance_without_proxy_restart() {
    let _serial = TEST_LOCK.lock().await;
    let bytes = b"abc";
    let (_directory, path) = payload("direct-ack-conflict", bytes);
    let cloud = MockServer::start();
    let object = MockServer::start();
    mount_discovery(&cloud, true);
    let create = cloud.mock(|when, then| {
        when.method(POST).path("/api/v1/uploads");
        then.status(200).json_body(json!({
            "clip_id": "c1", "upload_id": "u1", "mode": "chunked",
            "part_size_bytes": 3, "single_put_url": null,
            "parts_url_template": null,
            "direct_part_presign_url_template": "/api/v1/uploads/u1/parts/{part_number}/direct-presign",
            "direct_part_ack_url_template": "/api/v1/uploads/u1/parts/{part_number}/direct-ack"
        }));
    });
    mount_progress(
        &cloud,
        "u1",
        "c1",
        "chunked",
        "uploading",
        bytes.len(),
        vec![1],
    );
    cloud.mock(|when, then| {
        when.method(POST)
            .path("/api/v1/uploads/u1/parts/1/direct-presign");
        then.status(200).json_body(json!({
            "upload_id": "u1", "part_number": 1, "method": "PUT",
            "url": format!("{}/part", object.base_url()),
            "expires_at": "2030-01-01T00:00:00Z", "expected_size_bytes": 3,
            "headers": []
        }));
    });
    object.mock(|when, then| {
        when.method(PUT).path("/part");
        then.status(200).header("ETag", "\"etag\"");
    });
    cloud.mock(|when, then| {
        when.method(POST)
            .path("/api/v1/uploads/u1/parts/1/direct-ack");
        then.status(409)
            .json_body(json!({"error": "metadata conflict"}));
    });

    let error = ReqwestUploadTransport::new()
        .unwrap()
        .upload_file_with_progress(
            &api(&cloud),
            &CloudCredential::new(TOKEN),
            &request(bytes),
            None,
            &path,
            &UploadCancellation::default(),
            |_| {},
        )
        .await
        .unwrap_err();

    assert!(error.to_string().contains("Retry the upload"));
    create.assert_hits(1);
}

#[tokio::test]
async fn cancellation_interrupts_retry_after_without_an_extra_attempt() {
    let _serial = TEST_LOCK.lock().await;
    let bytes = b"abc";
    let (_directory, path) = payload("cancel-retry", bytes);
    let server = MockServer::start();
    mount_discovery(&server, false);
    server.mock(|when, then| {
        when.method(POST).path("/api/v1/uploads");
        then.status(200).json_body(json!({
            "clip_id": "c1", "upload_id": "u1", "mode": "chunked",
            "part_size_bytes": 3, "single_put_url": null,
            "parts_url_template": null
        }));
    });
    mount_progress(
        &server,
        "u1",
        "c1",
        "chunked",
        "uploading",
        bytes.len(),
        vec![1],
    );
    let failed = server.mock(|when, then| {
        when.method(PUT).path("/api/v1/uploads/u1/parts/1");
        then.status(503)
            .header("Retry-After", "30")
            .json_body(json!({"error": "later"}));
    });
    let cancellation = UploadCancellation::default();
    let task_cancellation = cancellation.clone();
    let task_api = api(&server);
    let task_path = path.clone();
    let task = tokio::spawn(async move {
        ReqwestUploadTransport::new()
            .unwrap()
            .upload_file_with_progress(
                &task_api,
                &CloudCredential::new(TOKEN),
                &request(bytes),
                None,
                &task_path,
                &task_cancellation,
                |_| {},
            )
            .await
    });
    for _ in 0..100 {
        if failed.hits() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(failed.hits(), 1);
    cancellation.cancel();

    assert_eq!(
        task.await.unwrap().unwrap_err(),
        CloudProtocolError::Canceled
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    failed.assert_hits(1);
}

#[tokio::test]
async fn cancellation_interrupts_an_in_flight_http_request() {
    let _serial = TEST_LOCK.lock().await;
    let bytes = b"abc";
    let (_directory, path) = payload("cancel-http", bytes);
    let server = MockServer::start();
    let discovery = server.mock(|when, then| {
        when.method(GET).path("/.well-known/clipline-cloud");
        then.status(200)
            .delay(Duration::from_secs(5))
            .json_body(json!({
                "name": "Clipline Cloud", "api_version": "v1",
                "server_version": "1.0.0", "min_client_version": "0.1.0",
                "public_url": server.base_url(),
                "features": {
                    "single_put_upload": true, "chunked_upload": true,
                    "direct_s3_upload": false, "public_sharing": true,
                    "clip_markers": true, "max_upload_size_bytes": 1000000
                }
            }));
    });
    let cancellation = UploadCancellation::default();
    let task_cancellation = cancellation.clone();
    let task_api = api(&server);
    let task = tokio::spawn(async move {
        ReqwestUploadTransport::new()
            .unwrap()
            .upload_file_with_progress(
                &task_api,
                &CloudCredential::new(TOKEN),
                &request(bytes),
                None,
                &path,
                &task_cancellation,
                |_| {},
            )
            .await
    });
    for _ in 0..100 {
        if discovery.hits() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(discovery.hits(), 1);
    cancellation.cancel();

    assert_eq!(
        task.await.unwrap().unwrap_err(),
        CloudProtocolError::Canceled
    );
}

#[tokio::test]
async fn process_wide_two_worker_limit_cancels_a_waiter_before_network_io() {
    let _serial = TEST_LOCK.lock().await;
    assert_eq!(MAX_CONCURRENT_UPLOADS, 2);
    let bytes = b"abc";
    let (_first_directory, first_path) = payload("worker-first", bytes);
    let first_server = MockServer::start();
    mount_discovery(&first_server, false);
    mount_single_create(&first_server, "u1", "c1", bytes.len());
    mount_progress(
        &first_server,
        "u1",
        "c1",
        "single_put",
        "uploading",
        bytes.len(),
        vec![],
    );
    let delayed_put = first_server.mock(|when, then| {
        when.method(PUT).path("/api/v1/uploads/u1/content");
        then.status(200)
            .delay(Duration::from_millis(500))
            .json_body(progress_json(
                "u1",
                "c1",
                "single_put",
                "completed",
                3,
                3,
                vec![],
            ));
    });
    let mut workers = Vec::new();
    for _ in 0..2 {
        let worker_api = api(&first_server);
        let worker_path = first_path.clone();
        workers.push(tokio::spawn(async move {
            ReqwestUploadTransport::new()
                .unwrap()
                .upload_file_with_progress(
                    &worker_api,
                    &CloudCredential::new(TOKEN),
                    &request(bytes),
                    None,
                    &worker_path,
                    &UploadCancellation::default(),
                    |_| {},
                )
                .await
        }));
    }
    for _ in 0..100 {
        if delayed_put.hits() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(delayed_put.hits(), 2);

    let (_third_directory, third_path) = payload("worker-third", bytes);
    let third_server = MockServer::start();
    let third_discovery = mount_discovery(&third_server, false);
    let third_cancellation = UploadCancellation::default();
    let task_cancellation = third_cancellation.clone();
    let third_api = api(&third_server);
    let third = tokio::spawn(async move {
        ReqwestUploadTransport::new()
            .unwrap()
            .upload_file_with_progress(
                &third_api,
                &CloudCredential::new(TOKEN),
                &request(bytes),
                None,
                &third_path,
                &task_cancellation,
                |_| {},
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    third_discovery.assert_hits(0);
    third_cancellation.cancel();
    assert_eq!(
        third.await.unwrap().unwrap_err(),
        CloudProtocolError::Canceled
    );

    for worker in workers {
        worker.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn authenticated_upload_template_cannot_cross_origins() {
    let _serial = TEST_LOCK.lock().await;
    let bytes = b"abc";
    let (_directory, path) = payload("cross-origin", bytes);
    let server = MockServer::start();
    let other = MockServer::start();
    mount_discovery(&server, false);
    server.mock(|when, then| {
        when.method(POST).path("/api/v1/uploads");
        then.status(200).json_body(json!({
            "clip_id": "c1", "upload_id": "u1", "mode": "single_put",
            "part_size_bytes": 3,
            "single_put_url": format!("{}/stolen", other.base_url()),
            "parts_url_template": null
        }));
    });
    mount_progress(
        &server,
        "u1",
        "c1",
        "single_put",
        "uploading",
        bytes.len(),
        vec![],
    );
    let stolen = other.mock(|when, then| {
        when.method(PUT).path("/stolen");
        then.status(200);
    });

    let error = ReqwestUploadTransport::new()
        .unwrap()
        .upload_file_with_progress(
            &api(&server),
            &CloudCredential::new(TOKEN),
            &request(bytes),
            None,
            &path,
            &UploadCancellation::default(),
            |_| {},
        )
        .await
        .unwrap_err();

    assert!(error.to_string().contains("configured cloud origin"));
    stolen.assert_hits(0);
}

#[test]
fn public_transport_bounds_match_the_pinned_contract() {
    assert_eq!(MAX_CONCURRENT_UPLOADS, 2);
    assert_eq!(MAX_UPLOAD_PART_BYTES, 64 * 1024 * 1024);
    assert_eq!(UPLOAD_PUT_MAX_ATTEMPTS, 3);
}
