use clipline_library::http::{CloudMediaProbe, ReqwestCloudProtocol};
use clipline_library::protocol::{
    CloudApiBase, CloudProtocolError, CreateDeviceTokenRequest, UpdateVisibilityRequest,
};
use httpmock::prelude::*;

fn discovery_json() -> serde_json::Value {
    serde_json::json!({
        "name": "Clipline Cloud",
        "api_version": "v1",
        "server_version": "1.2.3",
        "min_client_version": "0.1.0",
        "public_url": "https://clips.example/",
        "features": {
            "single_put_upload": true,
            "chunked_upload": true,
            "direct_s3_upload": false,
            "public_sharing": true,
            "clip_markers": true,
            "max_upload_size_bytes": 1073741824u64
        }
    })
}

fn user_json() -> serde_json::Value {
    serde_json::json!({
        "id": "user-1",
        "username": "dain",
        "display_name": "Dain",
        "email": null,
        "bio": null,
        "avatar_url": null,
        "role": "user",
        "is_disabled": false,
        "storage_bytes": 12,
        "storage_quota_bytes": 100,
        "created_at": "2026-08-02T00:00:00Z",
        "updated_at": "2026-08-02T00:00:00Z",
        "last_login_at": null
    })
}

fn clip_json(visibility: &str, public_url: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "id": "clip-1",
        "client_clip_id": "local-1",
        "title": "Clip",
        "description": null,
        "game_name": null,
        "game_id": null,
        "game_executable": null,
        "source_type": "replay",
        "recorded_at": null,
        "uploaded_at": "2026-08-02T00:00:00Z",
        "duration_ms": 5000,
        "file_size_bytes": 42,
        "width": 1920,
        "height": 1080,
        "fps": 60.0,
        "container": "mp4",
        "video_codec": "h264",
        "audio_codec": "opus",
        "checksum_sha256": null,
        "visibility": visibility,
        "status": "ready",
        "public_share_id": null,
        "public_url": public_url,
        "view_count": 0,
        "markers": [],
        "created_at": "2026-08-02T00:00:00Z",
        "updated_at": "2026-08-02T00:00:00Z"
    })
}

#[tokio::test]
async fn connection_calls_preserve_the_base_prefix_and_auth_boundary() {
    let server = MockServer::start();
    let discovery = server.mock(|when, then| {
        when.method(GET)
            .path("/clipline/.well-known/clipline-cloud");
        then.status(200).json_body(discovery_json());
    });
    let token = server.mock(|when, then| {
        when.method(POST)
            .path("/clipline/api/v1/auth/device-token")
            .json_body(serde_json::json!({
                "username": "dain",
                "password": "secret",
                "name": "Clipline Desktop"
            }));
        then.status(200).json_body(serde_json::json!({
            "token": "device-secret",
            "device_token": {
                "id": "device-1",
                "name": "Clipline Desktop",
                "created_at": "2026-08-02T00:00:00Z",
                "last_used_at": null,
                "expires_at": null,
                "revoked_at": null
            }
        }));
    });
    let profile = server.mock(|when, then| {
        when.method(GET)
            .path("/clipline/api/v1/auth/me")
            .header("authorization", "Bearer device-secret");
        then.status(200).json_body(serde_json::json!({
            "user": user_json(),
            "auth_kind": "device_token",
            "csrf_token": null
        }));
    });
    let base = CloudApiBase::parse(&server.url("/clipline/"), true).unwrap();
    let client = ReqwestCloudProtocol::new(base).unwrap();

    let discovered = client.discovery().await.unwrap();
    assert_eq!(discovered.api_version, "v1");
    let created = client
        .create_device_token(&CreateDeviceTokenRequest {
            username: "dain".into(),
            password: "secret".into(),
            name: "Clipline Desktop".into(),
        })
        .await
        .unwrap();
    let me = client.me(&created.token).await.unwrap();
    assert_eq!(me.user.id, "user-1");
    discovery.assert_hits(1);
    token.assert_hits(1);
    profile.assert_hits(1);
}

#[tokio::test]
async fn clip_status_visibility_and_media_probe_are_typed_and_bounded() {
    let server = MockServer::start();
    let get_clip = server.mock(|when, then| {
        when.method(GET)
            .path("/base/api/v1/clips/clip-1")
            .header("authorization", "Bearer token");
        then.status(200).json_body(clip_json("private", None));
    });
    let visibility = server.mock(|when, then| {
        when.method(POST)
            .path("/base/api/v1/clips/clip-1/visibility")
            .header("authorization", "Bearer token")
            .json_body(serde_json::json!({ "visibility": "public" }));
        then.status(200)
            .json_body(clip_json("public", Some("https://clips.example/c/1")));
    });
    let media = server.mock(|when, then| {
        when.method(GET)
            .path("/base/api/v1/clips/clip-1/media")
            .header("authorization", "Bearer token")
            .header("range", "bytes=0-0")
            .header("accept-encoding", "identity");
        then.status(206)
            .header("content-range", "bytes 0-0/42")
            .body(vec![0]);
    });
    let base = CloudApiBase::parse(&server.url("/base/"), true).unwrap();
    let client = ReqwestCloudProtocol::new(base).unwrap();

    assert_eq!(
        client.get_clip("token", "clip-1").await.unwrap().id,
        "clip-1"
    );
    let updated = client
        .update_visibility(
            "token",
            "clip-1",
            &UpdateVisibilityRequest {
                visibility: "public".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.visibility, "public");
    assert_eq!(
        client.probe_media("token", "clip-1").await.unwrap(),
        CloudMediaProbe {
            response_bytes: 1,
            total_size_bytes: Some(42)
        }
    );
    get_clip.assert_hits(1);
    visibility.assert_hits(1);
    media.assert_hits(1);
}

#[tokio::test]
async fn protocol_errors_do_not_follow_redirects_and_preserve_not_found() {
    let server = MockServer::start();
    let redirected = server.mock(|when, then| {
        when.method(GET).path("/.well-known/clipline-cloud");
        then.status(302).header("location", "/elsewhere");
    });
    let target = server.mock(|when, then| {
        when.method(GET).path("/elsewhere");
        then.status(200).json_body(discovery_json());
    });
    let client = ReqwestCloudProtocol::new(
        CloudApiBase::parse(&format!("{}/", server.base_url()), true).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        client.discovery().await,
        Err(CloudProtocolError::Api {
            status: reqwest::StatusCode::FOUND,
            ..
        })
    ));
    redirected.assert_hits(1);
    target.assert_hits(0);

    let missing_server = MockServer::start();
    missing_server.mock(|when, then| {
        when.method(GET).path("/api/v1/clips/missing");
        then.status(404).body("not found");
    });
    let client = ReqwestCloudProtocol::new(
        CloudApiBase::parse(&format!("{}/", missing_server.base_url()), true).unwrap(),
    )
    .unwrap();
    assert!(client
        .get_clip("token", "missing")
        .await
        .unwrap_err()
        .is_not_found());
}

#[tokio::test]
async fn media_probe_rejects_empty_or_range_ignoring_responses() {
    let empty_server = MockServer::start();
    empty_server.mock(|when, then| {
        when.method(GET).path("/api/v1/clips/clip-1/media");
        then.status(206).body(Vec::<u8>::new());
    });
    let client = ReqwestCloudProtocol::new(
        CloudApiBase::parse(&format!("{}/", empty_server.base_url()), true).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        client.probe_media("token", "clip-1").await,
        Err(CloudProtocolError::InvalidUpload(_))
    ));

    let oversized_server = MockServer::start();
    oversized_server.mock(|when, then| {
        when.method(GET).path("/api/v1/clips/clip-1/media");
        then.status(200).body(vec![0, 1]);
    });
    let client = ReqwestCloudProtocol::new(
        CloudApiBase::parse(&format!("{}/", oversized_server.base_url()), true).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        client.probe_media("token", "clip-1").await,
        Err(CloudProtocolError::Http(_))
    ));
}
