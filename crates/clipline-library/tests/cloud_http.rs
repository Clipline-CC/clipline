use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use clipline_library::cache::{
    AccountPublicationGuard, AvailableSpacePort, CloudAssetRequest, CloudCache, CloudCacheError,
    CloudCancellation,
};
use clipline_library::cache_identity::{
    CloudAccountFence, CloudAssetKey, CloudAssetKind, CloudCacheNamespace,
};
use clipline_library::http as cloud_http;
use clipline_library::ports::{CloudCredential, CloudRequestFence, CloudTransport};
use clipline_library::{
    CloudAccountGeneration, CloudAccountKey, CloudAccountSnapshot, CloudListQuery,
    CloudListTransportRequest, CloudServiceAccount, CloudWorkToken, ForegroundGeneration,
    RequestGeneration, WindowAttachmentGeneration, WindowWorkToken,
};
use httpmock::prelude::*;

#[tokio::test]
async fn control_json_is_bounded_and_redirects_are_not_followed() {
    let server = MockServer::start();
    let redirect = server.mock(|when, then| {
        when.method(GET).path("/redirect");
        then.status(302).header("location", "/payload");
    });
    let payload = server.mock(|when, then| {
        when.method(GET).path("/payload");
        then.status(200)
            .json_body(serde_json::json!({ "ok": true }));
    });

    let response = cloud_http::control_client()
        .unwrap()
        .get(server.url("/redirect"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::FOUND);
    redirect.assert_hits(1);
    payload.assert_hits(0);
}

#[tokio::test]
async fn bounded_json_and_error_text_use_the_pinned_control_limits() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/json");
        then.status(200)
            .json_body(serde_json::json!({ "ok": true }));
    });
    let response = cloud_http::control_client()
        .unwrap()
        .get(server.url("/json"))
        .send()
        .await
        .unwrap();
    let value: serde_json::Value = cloud_http::bounded_json(response, "control response", || false)
        .await
        .unwrap();
    assert_eq!(value["ok"], true);

    server.mock(|when, then| {
        when.method(GET).path("/error");
        then.status(500).body("nope");
    });
    let response = cloud_http::control_client()
        .unwrap()
        .get(server.url("/error"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        cloud_http::bounded_error_message(response, "cloud error").await,
        "nope"
    );

    server.mock(|when, then| {
        when.method(GET).path("/json-error");
        then.status(409)
            .json_body(serde_json::json!({ "error": "upload already exists" }));
    });
    let response = cloud_http::control_client()
        .unwrap()
        .get(server.url("/json-error"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        cloud_http::bounded_error_message(response, "cloud error").await,
        "upload already exists"
    );
}

#[tokio::test]
async fn declared_and_streamed_overflow_fail_closed() {
    let server = MockServer::start();
    let declared = server.mock(|when, then| {
        when.method(GET).path("/declared");
        then.status(200)
            .header("content-length", "9")
            .body("123456789");
    });
    let response = cloud_http::stream_client()
        .unwrap()
        .get(server.url("/declared"))
        .send()
        .await
        .unwrap();
    assert!(matches!(
        cloud_http::response_bytes_limited(response, 8, "asset", || false).await,
        Err(cloud_http::CloudHttpError::TooLarge { maximum: 8, .. })
    ));
    declared.assert_hits(1);

    let streamed = server.mock(|when, then| {
        when.method(GET).path("/streamed");
        then.status(200).body("123456789");
    });
    let response = cloud_http::stream_client()
        .unwrap()
        .get(server.url("/streamed"))
        .send()
        .await
        .unwrap();
    assert!(matches!(
        cloud_http::response_bytes_limited(response, 8, "asset", || false).await,
        Err(cloud_http::CloudHttpError::TooLarge { maximum: 8, .. })
    ));
    streamed.assert_hits(1);
}

#[tokio::test]
async fn cancellation_is_checked_before_accepting_each_body_chunk() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/asset");
        then.status(200).body("body");
    });
    let response = cloud_http::stream_client()
        .unwrap()
        .get(server.url("/asset"))
        .send()
        .await
        .unwrap();
    let canceled = AtomicBool::new(true);
    assert!(matches!(
        cloud_http::response_bytes_limited(response, 8, "asset", || {
            canceled.load(Ordering::Relaxed)
        })
        .await,
        Err(cloud_http::CloudHttpError::Canceled { .. })
    ));
}

#[test]
fn missing_is_accepted_only_for_optional_assets() {
    assert!(cloud_http::successful_or_missing(
        reqwest::StatusCode::OK,
        false
    ));
    assert!(cloud_http::successful_or_missing(
        reqwest::StatusCode::NOT_FOUND,
        true
    ));
    assert!(!cloud_http::successful_or_missing(
        reqwest::StatusCode::NOT_FOUND,
        false
    ));
}

#[test]
fn asset_transport_rejects_plain_http_nonlocal_hosts() {
    assert!(cloud_http::ReqwestAssetDownload::new("http://example.com", "secret").is_err());
    assert!(cloud_http::ReqwestAssetDownload::new("http://192.168.1.20", "secret").is_ok());
    assert!(cloud_http::ReqwestAssetDownload::new("http://[::1]", "secret").is_ok());
    assert!(cloud_http::ReqwestAssetDownload::new("https://example.com", "secret").is_ok());
}

#[tokio::test]
async fn concrete_transport_forwards_query_and_maps_the_pinned_wire_shape() {
    struct CurrentFence(CloudWorkToken);
    impl CloudRequestFence for CurrentFence {
        fn is_current(&self, token: &CloudWorkToken) -> bool {
            &self.0 == token
        }
    }

    let server = MockServer::start();
    let list = server.mock(|when, then| {
        when.method(GET)
            .path("/api/v1/clips")
            .query_param("sort", "title_asc")
            .query_param("q", "ranked")
            .query_param("page", "2")
            .query_param("page_size", "60")
            .header("authorization", "Bearer secret");
        then.status(200).json_body(serde_json::json!({
            "page": 2,
            "page_size": 60,
            "clips": [{
                "id": "remote-1",
                "client_clip_id": "local-1",
                "title": "One",
                "source_type": "replay",
                "uploaded_at": "2026-08-02T01:02:03Z",
                "duration_ms": 1500,
                "file_size_bytes": 2048,
                "visibility": "public",
                "status": "ready",
                "public_url": "https://clips.example/clip/remote-1",
                "updated_at": "2026-08-02T02:03:04Z"
            }]
        }));
    });
    let account = CloudServiceAccount {
        snapshot: CloudAccountSnapshot {
            account_key: CloudAccountKey::new("account-1").unwrap(),
            generation: CloudAccountGeneration::new(2),
            connected: true,
            host_url: server.base_url(),
            public_url: None,
            username: None,
            display_name: None,
            user_id: Some("user-1".into()),
            default_visibility: "private".into(),
            delete_local_after_upload: false,
            auto_upload_rules: false,
        },
        credential_target: Some("credential-1".into()),
        local_paths_by_clip_id: Default::default(),
    };
    let request = CloudListTransportRequest {
        page: 2,
        page_size: 60,
        query: CloudListQuery {
            sort: "title_asc".into(),
            query: Some("ranked".into()),
            ..CloudListQuery::default()
        },
    };
    let transport = cloud_http::ReqwestCloudTransport::new().unwrap();
    let token = CloudWorkToken {
        window: WindowWorkToken {
            attachment: WindowAttachmentGeneration::new(1),
            foreground: ForegroundGeneration::new(1),
            request: RequestGeneration::new(1),
        },
        account_key: account.snapshot.account_key.clone(),
        account_generation: account.snapshot.generation,
    };
    let fence = CurrentFence(token.clone());
    let response = transport
        .list(
            &account,
            &CloudCredential::new("secret"),
            &request,
            &fence,
            &token,
        )
        .await
        .unwrap();
    assert_eq!((response.page, response.page_size), (2, 60));
    assert_eq!(response.clips[0].remote_clip_id, "remote-1");
    assert_eq!(response.clips[0].source_type.as_deref(), Some("replay"));
    assert!(response.clips[0].updated_at_unix > 0);
    list.assert_hits(1);
}

#[test]
fn concrete_asset_transport_streams_into_the_bounded_cache_without_redirects() {
    struct Space;
    impl AvailableSpacePort for Space {
        fn available_bytes(&self, _cache_root: &Path) -> Result<u64, CloudCacheError> {
            Ok(u64::MAX)
        }
    }
    struct Gate;
    impl AccountPublicationGuard for Gate {
        fn is_current(&self, _account: &CloudAccountFence) -> bool {
            true
        }

        fn publish_if_current(
            &self,
            _account: &CloudAccountFence,
            publication: &mut dyn FnMut() -> Result<(), CloudCacheError>,
        ) -> Result<(), CloudCacheError> {
            publication()
        }
    }

    let server = MockServer::start();
    let asset = server.mock(|when, then| {
        when.method(GET)
            .path("/api/v1/clips/remote-1/thumbnail")
            .header("authorization", "Bearer secret");
        then.status(200).body([0xff, 0xd8, 0xff, 0xd9]);
    });
    let redirected_target = server.mock(|when, then| {
        when.method(GET).path("/redirected-thumbnail");
        then.status(200).body([0xff, 0xd8, 0xff, 0xd9]);
    });
    let redirect = server.mock(|when, then| {
        when.method(GET).path("/api/v1/clips/remote-2/thumbnail");
        then.status(302).header("location", "/redirected-thumbnail");
    });
    let account = CloudAccountFence {
        account_key: CloudAccountKey::new("account-1").unwrap(),
        account_generation: CloudAccountGeneration::new(1),
        cache_namespace: CloudCacheNamespace::derive(&server.base_url(), "user-1").unwrap(),
    };
    let dir = clipline_test_utils::TestDir::new("clipline-library", "cloud-http-asset");
    let cache = CloudCache::open(
        dir.path(),
        Arc::new(cloud_http::ReqwestAssetDownload::new(&server.base_url(), "secret").unwrap()),
        Arc::new(Space),
        Arc::new(Gate),
    )
    .unwrap();
    let cached = cache
        .get(
            CloudAssetRequest {
                account: account.clone(),
                asset: CloudAssetKey::new("remote-1", CloudAssetKind::Thumbnail, 1).unwrap(),
                expected_size_bytes: None,
            },
            &CloudCancellation::default(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        std::fs::read(cached.path()).unwrap(),
        [0xff, 0xd8, 0xff, 0xd9]
    );
    asset.assert_hits(1);

    let error = cache
        .get(
            CloudAssetRequest {
                account,
                asset: CloudAssetKey::new("remote-2", CloudAssetKind::Thumbnail, 1).unwrap(),
                expected_size_bytes: None,
            },
            &CloudCancellation::default(),
        )
        .unwrap_err();
    assert!(matches!(error, CloudCacheError::Download(_)));
    redirect.assert_hits(1);
    redirected_target.assert_hits(0);
}
