//! Post-upload readiness polling and cloud page URLs.
use super::*;

pub(crate) async fn wait_for_ready_clip(
    client: &CloudClient,
    token: &str,
    clip_id: &str,
) -> Result<ReadyClipOutcome, CloudApiError> {
    wait_for_ready_clip_with_policy(
        client,
        token,
        clip_id,
        READY_POLL_ATTEMPTS,
        READY_POLL_DELAY,
    )
    .await
}

pub(crate) async fn wait_for_ready_clip_with_policy(
    client: &CloudClient,
    token: &str,
    clip_id: &str,
    attempts: usize,
    delay: Duration,
) -> Result<ReadyClipOutcome, CloudApiError> {
    for attempt in 0..attempts {
        match bounded_cloud_get_clip(client, token, clip_id).await {
            Ok(clip) if clip.status == "ready" => return Ok(ReadyClipOutcome::Ready(clip)),
            Ok(clip) if clip.status == "failed" => return Ok(ReadyClipOutcome::Failed(clip)),
            Ok(_)
            | Err(CloudApiError::Api {
                status: reqwest::StatusCode::NOT_FOUND,
                ..
            }) => {}
            Err(error) => return Err(error),
        }
        if attempt + 1 < attempts && !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
    }
    Ok(ReadyClipOutcome::TimedOut)
}

pub(crate) async fn verify_ready_cloud_media(
    cloud: &CloudSettings,
    token: &str,
    remote_clip_id: &str,
) -> Result<(), String> {
    let url = cloud_clip_asset_url(cloud, remote_clip_id, "media")?;
    let client = reqwest::Client::builder()
        .connect_timeout(READY_MEDIA_PROBE_CONNECT_TIMEOUT)
        .timeout(READY_MEDIA_PROBE_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("create media verification client: {error}"))?;
    let mut response = client
        .get(url)
        .bearer_auth(token)
        .header(reqwest::header::RANGE, "bytes=0-0")
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .send()
        .await
        .map_err(|error| format!("request ready cloud media: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "ready cloud media returned HTTP {}",
            response.status()
        ));
    }
    let first_chunk = response
        .chunk()
        .await
        .map_err(|error| format!("read ready cloud media: {error}"))?;
    if first_chunk.as_ref().is_none_or(|bytes| bytes.is_empty()) {
        return Err("ready cloud media returned no bytes".to_string());
    }
    Ok(())
}

pub(crate) fn cloud_owner_clip_page_url(
    cloud: &CloudSettings,
    remote_clip_id: &str,
) -> Result<reqwest::Url, String> {
    let remote_clip_id = validate_cloud_cache_component(remote_clip_id, "remote clip id")?;
    let base = cloud.public_url.as_deref().unwrap_or(&cloud.host_url);
    let mut url = clipline_cloud_api::validate_cloud_host(base, true).map_err(cloud_error)?;
    url = url
        .join("clip/")
        .map_err(|error| format!("cloud clip page URL is invalid: {error}"))?;
    url.path_segments_mut()
        .map_err(|_| "cloud clip page URL cannot be a base".to_string())?
        .pop_if_empty()
        .push(remote_clip_id);
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    #[tokio::test]
    async fn readiness_poll_does_not_accept_processing_as_ready() {
        let server = MockServer::start();
        let response = clip_detail("remote-1", "private", "processing", None);
        let request = server.mock(|when, then| {
            when.method(GET).path("/api/v1/clips/remote-1");
            then.status(200)
                .header("content-type", "application/json")
                .json_body_obj(&response);
        });

        let outcome = wait_for_ready_clip_with_policy(
            &test_cloud_client(&server),
            "token",
            "remote-1",
            3,
            Duration::ZERO,
        )
        .await
        .unwrap();

        assert!(matches!(outcome, ReadyClipOutcome::TimedOut));
        request.assert_hits(3);
    }

    #[tokio::test]
    async fn readiness_poll_treats_remote_processing_failure_as_terminal() {
        let server = MockServer::start();
        let response = clip_detail("remote-1", "private", "failed", None);
        let request = server.mock(|when, then| {
            when.method(GET).path("/api/v1/clips/remote-1");
            then.status(200)
                .header("content-type", "application/json")
                .json_body_obj(&response);
        });

        let outcome = wait_for_ready_clip_with_policy(
            &test_cloud_client(&server),
            "token",
            "remote-1",
            3,
            Duration::ZERO,
        )
        .await
        .unwrap();

        assert!(matches!(outcome, ReadyClipOutcome::Failed(clip) if clip.status == "failed"));
        request.assert_hits(1);
    }

    #[tokio::test]
    async fn readiness_poll_returns_only_an_explicitly_ready_clip() {
        let server = MockServer::start();
        let response = clip_detail("remote-1", "private", "ready", None);
        let request = server.mock(|when, then| {
            when.method(GET).path("/api/v1/clips/remote-1");
            then.status(200)
                .header("content-type", "application/json")
                .json_body_obj(&response);
        });

        let outcome = wait_for_ready_clip_with_policy(
            &test_cloud_client(&server),
            "token",
            "remote-1",
            3,
            Duration::ZERO,
        )
        .await
        .unwrap();

        assert!(matches!(outcome, ReadyClipOutcome::Ready(clip) if clip.status == "ready"));
        request.assert_hits(1);
    }

    #[tokio::test]
    async fn ready_media_probe_requires_retrievable_nonempty_content() {
        let server = MockServer::start();
        let media = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/clips/remote-1/media")
                .header("authorization", "Bearer token")
                .header("range", "bytes=0-0");
            then.status(206).body("x");
        });
        let cloud = CloudSettings {
            host_url: server.base_url(),
            ..CloudSettings::default()
        };

        verify_ready_cloud_media(&cloud, "token", "remote-1")
            .await
            .unwrap();

        media.assert_hits(1);
    }

    #[tokio::test]
    async fn ready_media_probe_rejects_empty_and_failed_responses() {
        let empty_server = MockServer::start();
        empty_server.mock(|when, then| {
            when.method(GET).path("/api/v1/clips/remote-1/media");
            then.status(206);
        });
        let empty_cloud = CloudSettings {
            host_url: empty_server.base_url(),
            ..CloudSettings::default()
        };
        let empty_error = verify_ready_cloud_media(&empty_cloud, "token", "remote-1")
            .await
            .expect_err("empty media is not durable");
        assert!(empty_error.contains("no bytes"), "{empty_error}");

        let failed_server = MockServer::start();
        failed_server.mock(|when, then| {
            when.method(GET).path("/api/v1/clips/remote-1/media");
            then.status(404);
        });
        let failed_cloud = CloudSettings {
            host_url: failed_server.base_url(),
            ..CloudSettings::default()
        };
        let failed_error = verify_ready_cloud_media(&failed_cloud, "token", "remote-1")
            .await
            .expect_err("missing media is not durable");
        assert!(failed_error.contains("404"), "{failed_error}");
    }

    #[tokio::test]
    async fn visibility_update_refreshes_canonical_public_url() {
        let server = MockServer::start();
        let stale_update = clip_detail("remote-1", "public", "ready", None);
        let refreshed = clip_detail(
            "remote-1",
            "public",
            "ready",
            Some("https://clips.example.com/c/c_share"),
        );
        let update = server.mock(|when, then| {
            when.method(POST)
                .path("/api/v1/clips/remote-1/visibility")
                .json_body_obj(&UpdateVisibilityRequest {
                    visibility: "public".to_string(),
                });
            then.status(200)
                .header("content-type", "application/json")
                .json_body_obj(&stale_update);
        });
        let refresh = server.mock(|when, then| {
            when.method(GET).path("/api/v1/clips/remote-1");
            then.status(200)
                .header("content-type", "application/json")
                .json_body_obj(&refreshed);
        });

        let clip = update_cloud_clip_visibility(
            &test_cloud_client(&server),
            "token",
            "remote-1",
            "public",
        )
        .await
        .expect("update and refresh visibility");

        assert_eq!(
            clip.public_url.as_deref(),
            Some("https://clips.example.com/c/c_share")
        );
        update.assert();
        refresh.assert();
    }

    #[tokio::test]
    async fn visibility_update_preserves_post_detail_if_refresh_fails() {
        let server = MockServer::start();
        let updated = clip_detail(
            "remote-1",
            "unlisted",
            "ready",
            Some("https://clips.example.com/c/c_post_fallback"),
        );
        let update = server.mock(|when, then| {
            when.method(POST).path("/api/v1/clips/remote-1/visibility");
            then.status(200)
                .header("content-type", "application/json")
                .json_body_obj(&updated);
        });
        let refresh = server.mock(|when, then| {
            when.method(GET).path("/api/v1/clips/remote-1");
            then.status(503).body("try again later");
        });

        let clip = update_cloud_clip_visibility(
            &test_cloud_client(&server),
            "token",
            "remote-1",
            "unlisted",
        )
        .await
        .expect("successful visibility update remains successful");

        assert_eq!(
            clip.public_url.as_deref(),
            Some("https://clips.example.com/c/c_post_fallback")
        );
        update.assert();
        refresh.assert();
    }

    #[tokio::test]
    async fn visibility_update_keeps_url_less_success_recoverable_if_refresh_fails() {
        let server = MockServer::start();
        let updated = clip_detail("remote-1", "public", "ready", None);
        let update = server.mock(|when, then| {
            when.method(POST).path("/api/v1/clips/remote-1/visibility");
            then.status(200)
                .header("content-type", "application/json")
                .json_body_obj(&updated);
        });
        let refresh = server.mock(|when, then| {
            when.method(GET).path("/api/v1/clips/remote-1");
            then.status(503).body("try again later");
        });

        let error = update_cloud_clip_visibility(
            &test_cloud_client(&server),
            "token",
            "remote-1",
            "public",
        )
        .await
        .expect_err("a URL-less public update must remain recoverable");

        assert!(
            error
                .to_string()
                .contains("refreshing the canonical public URL failed"),
            "{error}"
        );
        update.assert();
        refresh.assert();
    }

    #[test]
    fn cloud_owner_clip_page_url_uses_configured_origin_and_one_safe_segment() {
        let public_cloud = CloudSettings {
            host_url: "https://api.example.com/base/".into(),
            public_url: Some("https://clips.example.com/cloud/".into()),
            ..CloudSettings::default()
        };
        assert_eq!(
            cloud_owner_clip_page_url(&public_cloud, "remote-1_ABC")
                .expect("public clip page")
                .as_str(),
            "https://clips.example.com/cloud/clip/remote-1_ABC"
        );

        let private_cloud = CloudSettings {
            host_url: "http://127.0.0.1:8080/root/".into(),
            ..CloudSettings::default()
        };
        assert_eq!(
            cloud_owner_clip_page_url(&private_cloud, "remote-2")
                .expect("private clip page")
                .as_str(),
            "http://127.0.0.1:8080/root/clip/remote-2"
        );
        for invalid in ["", "../escape", "remote/escape", "remote?redirect=evil"] {
            assert!(
                cloud_owner_clip_page_url(&public_cloud, invalid).is_err(),
                "remote id must be one safe segment: {invalid}"
            );
        }
    }

}