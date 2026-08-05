use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clipline_games::osu::{OsuClientSecret, MAX_OSU_CLIENT_SECRET_BYTES};
use clipline_games::osu_http::{
    OsuHttpClient, OsuHttpConfig, OsuHttpErrorKind, OsuHttpOwner, OsuRequestFence,
    OSU_CONTROL_JSON_MAX_BYTES, OSU_RECENT_PAGE_LIMIT, OSU_RECENT_SCORE_CEILING,
};
use clipline_settings::OsuAccountGeneration;
use httpmock::prelude::*;
use httpmock::Mock;

struct TestFence {
    owner: OsuHttpOwner,
    checks: AtomicUsize,
    current_through: usize,
    canceled: AtomicBool,
    cancel_notify: tokio::sync::Notify,
}

impl TestFence {
    fn current(owner: OsuHttpOwner) -> Self {
        Self {
            owner,
            checks: AtomicUsize::new(0),
            current_through: usize::MAX,
            canceled: AtomicBool::new(false),
            cancel_notify: tokio::sync::Notify::new(),
        }
    }

    fn changes_after(owner: OsuHttpOwner, current_through: usize) -> Self {
        Self {
            current_through,
            ..Self::current(owner)
        }
    }

    fn cancel(&self) {
        self.canceled.store(true, Ordering::SeqCst);
        self.cancel_notify.notify_waiters();
    }
}

impl OsuRequestFence for TestFence {
    fn is_current(&self, owner: OsuHttpOwner) -> bool {
        let check = self.checks.fetch_add(1, Ordering::SeqCst) + 1;
        owner == self.owner
            && check <= self.current_through
            && !self.canceled.load(Ordering::SeqCst)
    }

    fn cancelled<'a>(
        &'a self,
        _owner: OsuHttpOwner,
    ) -> clipline_games::osu_http::OsuCancellationFuture<'a> {
        Box::pin(async move {
            if self.canceled.load(Ordering::SeqCst) {
                return;
            }
            self.cancel_notify.notified().await;
        })
    }
}

fn owner(generation: u64) -> OsuHttpOwner {
    OsuHttpOwner::new(OsuAccountGeneration::new(generation).unwrap())
}

fn config(owner: OsuHttpOwner) -> OsuHttpConfig {
    OsuHttpConfig::new(
        owner,
        "12345".into(),
        "67890".into(),
        OsuClientSecret::new("test-secret".into()).unwrap(),
    )
    .unwrap()
}

fn client(server: &MockServer, timeout: Duration) -> OsuHttpClient {
    OsuHttpClient::with_endpoints(
        reqwest::Url::parse(&format!("{}/oauth/token", server.base_url())).unwrap(),
        reqwest::Url::parse(&format!("{}/api/v2/", server.base_url())).unwrap(),
        timeout,
    )
    .unwrap()
}

fn token(server: &MockServer) -> Mock<'_> {
    server.mock(|when, then| {
        when.method(POST)
            .path("/oauth/token")
            .body_contains("client_id=12345")
            .body_contains("client_secret=test-secret")
            .body_contains("grant_type=client_credentials")
            .body_contains("scope=public");
        then.status(200)
            .header("content-type", "application/json")
            .json_body(serde_json::json!({ "access_token": "access-token" }));
    })
}

fn raw_score(id: usize, passed: bool) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "passed": passed,
        "accuracy": 0.99,
        "max_combo": 500,
        "total_score": 1234567,
        "pp": if passed { Some(321.0) } else { None },
        "started_at": "2026-08-04T00:00:00Z",
        "ended_at": "2026-08-04T00:02:00Z",
        "rank": if passed { "S" } else { "F" },
        "mods": [{ "acronym": "HD" }],
        "beatmap": {
            "id": 12,
            "version": "Insane",
            "difficulty_rating": 5.5,
            "total_length": 120.0
        },
        "beatmapset": {
            "id": 34,
            "title": "Song",
            "artist": "Artist",
            "creator": "Mapper",
            "covers": { "list": "https://assets.ppy.sh/cover.jpg" }
        },
        "user": { "username": "player" }
    })
}

#[tokio::test]
async fn successful_fetch_preserves_shipping_shape_and_bounds() {
    let server = MockServer::start();
    let token = token(&server);
    let recent = server.mock(|when, then| {
        when.method(GET)
            .path("/api/v2/users/67890/scores/recent")
            .query_param("include_fails", "1")
            .query_param("legacy_only", "0")
            .query_param("mode", "osu")
            .query_param("limit", "100")
            .query_param("offset", "0")
            .header("authorization", "Bearer access-token")
            .header("x-api-version", "20220705");
        then.status(200)
            .header("content-type", "application/json")
            .json_body(serde_json::json!([raw_score(1, false), raw_score(2, true)]));
    });
    let owner = owner(7);
    let fence = TestFence::current(owner);

    let fetched = client(&server, Duration::from_secs(2))
        .fetch_recent_scores(&config(owner), None, &fence)
        .await
        .unwrap();

    token.assert();
    recent.assert();
    assert_eq!(fetched.owner, owner);
    assert_eq!(fetched.user_id, "67890");
    assert_eq!(fetched.username.as_deref(), Some("player"));
    assert_eq!(fetched.scores.len(), 2);
    assert_eq!(fetched.failed_count, 1);
    assert_eq!(fetched.started_at_count, 2);
    assert_eq!(fetched.ended_at_count, 2);
    assert!(!fetched.pagination_ceiling_reached);
    assert_eq!(
        fetched.scores[0].url.as_deref(),
        Some("https://osu.ppy.sh/scores/osu/1")
    );
    assert_eq!(fetched.scores[0].mods, ["HD"]);
}

#[tokio::test]
async fn username_lookup_is_encoded_and_resolved_before_scores() {
    let server = MockServer::start();
    let _token = token(&server);
    let lookup = server.mock(|when, then| {
        when.method(GET)
            .path("/api/v2/users/@name%20with%20space/osu")
            .header("authorization", "Bearer access-token");
        then.status(200)
            .header("content-type", "application/json")
            .json_body(serde_json::json!({ "id": 42, "username": "Resolved" }));
    });
    let recent = server.mock(|when, then| {
        when.method(GET).path("/api/v2/users/42/scores/recent");
        then.status(200)
            .header("content-type", "application/json")
            .json_body(serde_json::json!([]));
    });
    let owner = owner(1);
    let config = OsuHttpConfig::new(
        owner,
        "12345".into(),
        "name with space".into(),
        OsuClientSecret::new("test-secret".into()).unwrap(),
    )
    .unwrap();

    let fetched = client(&server, Duration::from_secs(2))
        .fetch_recent_scores(&config, None, &TestFence::current(owner))
        .await
        .unwrap();

    lookup.assert();
    recent.assert();
    assert_eq!(fetched.user_id, "42");
    assert_eq!(fetched.username.as_deref(), Some("Resolved"));
}

#[tokio::test]
async fn oversized_and_malformed_responses_fail_closed() {
    let oversized_server = MockServer::start();
    let _token = token(&oversized_server);
    let _oversized = oversized_server.mock(|when, then| {
        when.method(GET).path("/api/v2/users/67890/scores/recent");
        then.status(200)
            .body(vec![b' '; OSU_CONTROL_JSON_MAX_BYTES + 1]);
    });
    let owner = owner(2);
    let error = client(&oversized_server, Duration::from_secs(5))
        .fetch_recent_scores(&config(owner), None, &TestFence::current(owner))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), OsuHttpErrorKind::TooLarge);

    let malformed_server = MockServer::start();
    let _token = token(&malformed_server);
    let _malformed = malformed_server.mock(|when, then| {
        when.method(GET).path("/api/v2/users/67890/scores/recent");
        then.status(200).body("{not-json");
    });
    let error = client(&malformed_server, Duration::from_secs(2))
        .fetch_recent_scores(&config(owner), None, &TestFence::current(owner))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), OsuHttpErrorKind::Malformed);
}

#[tokio::test]
async fn page_and_total_score_ceilings_are_exact() {
    let too_many_server = MockServer::start();
    let _token = token(&too_many_server);
    let too_many: Vec<_> = (0..=OSU_RECENT_PAGE_LIMIT)
        .map(|id| raw_score(id, true))
        .collect();
    let _recent = too_many_server.mock(|when, then| {
        when.method(GET).path("/api/v2/users/67890/scores/recent");
        then.status(200)
            .header("content-type", "application/json")
            .json_body(serde_json::Value::Array(too_many));
    });
    let owner = owner(3);
    let error = client(&too_many_server, Duration::from_secs(2))
        .fetch_recent_scores(&config(owner), None, &TestFence::current(owner))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), OsuHttpErrorKind::TooLarge);

    let ceiling_server = MockServer::start();
    let token = token(&ceiling_server);
    let page: Vec<_> = (0..OSU_RECENT_PAGE_LIMIT)
        .map(|id| raw_score(id, true))
        .collect();
    let recent = ceiling_server.mock(|when, then| {
        when.method(GET).path("/api/v2/users/67890/scores/recent");
        then.status(200)
            .header("content-type", "application/json")
            .json_body(serde_json::Value::Array(page));
    });
    let fetched = client(&ceiling_server, Duration::from_secs(5))
        .fetch_recent_scores(&config(owner), None, &TestFence::current(owner))
        .await
        .unwrap();
    token.assert();
    assert_eq!(
        recent.hits(),
        OSU_RECENT_SCORE_CEILING / OSU_RECENT_PAGE_LIMIT
    );
    assert_eq!(fetched.scores.len(), OSU_RECENT_SCORE_CEILING);
    assert!(fetched.pagination_ceiling_reached);
}

#[tokio::test]
async fn unauthorized_redirect_timeout_and_offline_are_typed() {
    let unauthorized = MockServer::start();
    let _token = unauthorized.mock(|when, then| {
        when.method(POST).path("/oauth/token");
        then.status(401).json_body(serde_json::json!({
            "error": "invalid secret test-secret"
        }));
    });
    let owner = owner(4);
    let error = client(&unauthorized, Duration::from_secs(2))
        .fetch_recent_scores(&config(owner), None, &TestFence::current(owner))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), OsuHttpErrorKind::Unauthorized);
    assert_eq!(error.status_code(), Some(401));
    assert!(!format!("{error:?}").contains("test-secret"));

    let redirect = MockServer::start();
    let destination = redirect.mock(|when, then| {
        when.method(GET).path("/must-not-follow");
        then.status(200);
    });
    let _token = redirect.mock(|when, then| {
        when.method(POST).path("/oauth/token");
        then.status(302).header(
            "location",
            format!("{}/must-not-follow", redirect.base_url()),
        );
    });
    let error = client(&redirect, Duration::from_secs(2))
        .fetch_recent_scores(&config(owner), None, &TestFence::current(owner))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), OsuHttpErrorKind::Remote);
    destination.assert_hits(0);

    let timeout_server = MockServer::start();
    let _slow = timeout_server.mock(|when, then| {
        when.method(POST).path("/oauth/token");
        then.status(200)
            .delay(Duration::from_millis(150))
            .json_body(serde_json::json!({ "access_token": "late" }));
    });
    let error = client(&timeout_server, Duration::from_millis(20))
        .fetch_recent_scores(&config(owner), None, &TestFence::current(owner))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), OsuHttpErrorKind::Timeout);

    let offline = OsuHttpClient::with_endpoints(
        reqwest::Url::parse("http://127.0.0.1:9/oauth/token").unwrap(),
        reqwest::Url::parse("http://127.0.0.1:9/api/v2/").unwrap(),
        Duration::from_millis(250),
    )
    .unwrap();
    let error = offline
        .fetch_recent_scores(&config(owner), None, &TestFence::current(owner))
        .await
        .unwrap_err();
    assert!(matches!(
        error.kind(),
        OsuHttpErrorKind::Offline | OsuHttpErrorKind::Timeout
    ));
}

#[tokio::test]
async fn account_change_and_cancellation_stop_before_stale_append() {
    let changed_server = MockServer::start();
    let token_mock = token(&changed_server);
    let recent = changed_server.mock(|when, then| {
        when.method(GET).path("/api/v2/users/67890/scores/recent");
        then.status(200)
            .json_body(serde_json::json!([raw_score(1, true)]));
    });
    let owner = owner(5);
    let error = client(&changed_server, Duration::from_secs(2))
        .fetch_recent_scores(&config(owner), None, &TestFence::changes_after(owner, 2))
        .await
        .unwrap_err();
    token_mock.assert();
    recent.assert_hits(0);
    assert_eq!(error.kind(), OsuHttpErrorKind::AccountChanged);

    let cancel_server = MockServer::start();
    let _token = token(&cancel_server);
    let _recent = cancel_server.mock(|when, then| {
        when.method(GET).path("/api/v2/users/67890/scores/recent");
        then.status(200)
            .delay(Duration::from_secs(2))
            .json_body(serde_json::json!([raw_score(1, true)]));
    });
    let client = Arc::new(client(&cancel_server, Duration::from_secs(5)));
    let config = Arc::new(config(owner));
    let fence = Arc::new(TestFence::current(owner));
    let task = {
        let client = Arc::clone(&client);
        let config = Arc::clone(&config);
        let fence = Arc::clone(&fence);
        tokio::spawn(async move {
            client
                .fetch_recent_scores(config.as_ref(), None, fence.as_ref())
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    fence.cancel();
    let error = task.await.unwrap().unwrap_err();
    assert_eq!(error.kind(), OsuHttpErrorKind::Canceled);
}

#[test]
fn config_and_errors_never_render_secrets() {
    let owner = owner(6);
    let secret = "s".repeat(MAX_OSU_CLIENT_SECRET_BYTES);
    let config = OsuHttpConfig::new(
        owner,
        "123".into(),
        "user".into(),
        OsuClientSecret::new(secret.clone()).unwrap(),
    )
    .unwrap();
    assert!(!format!("{config:?}").contains(&secret));
}
