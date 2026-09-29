//! Upload entry point: concurrency bound, source lease, and transport dispatch.
use super::*;

pub async fn upload_mp4_file_with_progress<F>(
    client: &CloudClient,
    device_token: &str,
    request: &CreateUploadRequest,
    description: Option<&str>,
    path: &Path,
    mut on_progress: F,
) -> CloudApiResult<UploadProgressResponse>
where
    F: FnMut(&UploadProgressResponse),
{
    let _upload_permit = UPLOAD_PERMITS.acquire().await.map_err(|_| {
        CloudApiError::InvalidUpload("cloud upload concurrency limiter is closed".to_string())
    })?;
    let _source_lease = UploadSourceLease::acquire(path)?;
    validate_upload_request_matches_file(request, path).await?;
    let authenticated_control =
        crate::bounded_http::control_client().map_err(CloudApiError::InvalidUpload)?;
    let authenticated_stream =
        crate::bounded_http::authenticated_stream_client().map_err(CloudApiError::InvalidUpload)?;
    let object_http =
        crate::bounded_http::object_stream_client().map_err(CloudApiError::InvalidUpload)?;
    let direct_s3_available = discover_direct_s3(client, authenticated_control)
        .await
        .unwrap_or(false);
    let transport = UploadTransport {
        client,
        authenticated_control,
        authenticated_stream,
        object_http,
        device_token,
    };

    let upload = create_upload(
        client,
        authenticated_control,
        device_token,
        request,
        description,
    )
    .await?;
    match upload_existing(
        transport,
        &upload,
        path,
        direct_s3_available,
        &mut on_progress,
    )
    .await
    {
        Ok(progress) => Ok(progress),
        Err(DirectUploadError::Fallback(_reason)) => {
            let upload = create_upload(
                client,
                authenticated_control,
                device_token,
                request,
                description,
            )
            .await?;
            upload_existing(transport, &upload, path, false, &mut on_progress)
                .await
                .map_err(DirectUploadError::into_cloud_error)
        }
        Err(error) => Err(error.into_cloud_error()),
    }
}

pub(crate) async fn upload_existing<F>(
    transport: UploadTransport<'_>,
    upload: &CreateUploadResponse,
    path: &Path,
    direct_s3_available: bool,
    on_progress: &mut F,
) -> Result<UploadProgressResponse, DirectUploadError>
where
    F: FnMut(&UploadProgressResponse),
{
    match upload.mode.as_str() {
        "single_put" => upload_single(
            transport.client,
            transport.authenticated_control,
            transport.authenticated_stream,
            transport.device_token,
            upload,
            path,
            on_progress,
        )
        .await
        .map_err(DirectUploadError::Cloud),
        "chunked" => {
            let progress = get_upload_progress(
                transport.client,
                transport.authenticated_control,
                transport.device_token,
                &upload.upload_id,
            )
            .await
            .map_err(DirectUploadError::Cloud)?;
            on_progress(&progress);

            let Some(presign_template) = upload.direct_part_presign_url_template.as_deref() else {
                return upload_chunked_proxy(transport, upload, path, progress, on_progress)
                    .await
                    .map_err(DirectUploadError::Cloud);
            };
            let Some(ack_template) = upload.direct_part_ack_url_template.as_deref() else {
                return upload_chunked_proxy(transport, upload, path, progress, on_progress)
                    .await
                    .map_err(DirectUploadError::Cloud);
            };

            if !direct_s3_available {
                return upload_chunked_proxy(transport, upload, path, progress, on_progress)
                    .await
                    .map_err(DirectUploadError::Cloud);
            }

            let templates = DirectPartTemplates {
                presign: presign_template,
                ack: ack_template,
            };
            upload_chunked_direct(transport, upload, path, progress, templates, on_progress).await
        }
        other => Err(DirectUploadError::Cloud(CloudApiError::InvalidUpload(
            format!("server returned unsupported upload mode {other:?}"),
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_cloud_api::sha256_hex;
    use httpmock::prelude::*;
    use serde_json::json;

    #[test]
    fn simultaneous_top_level_uploads_are_bounded() {
        assert_eq!(MAX_CONCURRENT_UPLOADS, 2);
        assert!(!UPLOAD_PERMITS.is_closed());
    }

    #[tokio::test]
    async fn discovery_without_direct_s3_uses_proxy_chunked_path() {
        let bytes = b"abcdef";
        let cloud = MockServer::start();
        mount_discovery(&cloud, false);
        mount_chunked_create(
            &cloud,
            "u1",
            "c1",
            Some("/api/v1/uploads/u1/parts/{part_number}/presign"),
            Some("/api/v1/uploads/u1/parts/{part_number}/ack"),
        );
        mount_progress(
            &cloud,
            "u1",
            "c1",
            "uploading",
            bytes.len() as u64,
            vec![1, 2],
        );
        let part1 = mount_proxy_part(&cloud, "u1", 1, "abc");
        let part2 = mount_proxy_part(&cloud, "u1", 2, "def");
        let complete = mount_complete(&cloud, "u1", "c1", bytes.len() as u64);

        let client = test_client(&cloud);
        let progress = upload_mp4_bytes_with_progress(
            &client,
            TOKEN,
            &upload_request(bytes),
            None,
            bytes,
            |_| {},
        )
        .await
        .expect("upload");

        assert_eq!(progress.status, "completed");
        part1.assert();
        part2.assert();
        complete.assert();
    }

    #[tokio::test]
    async fn proxy_part_retry_reopens_the_stream() {
        let bytes = b"abc";
        let cloud = MockServer::start();
        mount_discovery(&cloud, false);
        mount_chunked_create(&cloud, "u1", "c1", None, None);
        mount_progress(&cloud, "u1", "c1", "uploading", bytes.len() as u64, vec![1]);
        let failed_part = cloud.mock(|when, then| {
            when.method(PUT)
                .path("/api/v1/uploads/u1/parts/1")
                .header("content-length", "3")
                .header("x-clipline-part-sha256", sha256_hex(bytes))
                .body("abc");
            then.status(503)
                .json_body(json!({ "error": "temporarily unavailable" }));
        });
        let client = test_client(&cloud);

        let error = upload_mp4_bytes_with_progress(
            &client,
            TOKEN,
            &upload_request(bytes),
            None,
            bytes,
            |_| {},
        )
        .await
        .expect_err("all proxy attempts should fail");

        assert!(
            error.to_string().contains("temporarily unavailable"),
            "{error}"
        );
        failed_part.assert_hits(3);
    }

    #[tokio::test]
    async fn direct_s3_does_not_change_single_put_uploads() {
        let bytes = b"single body";
        let cloud = MockServer::start();
        mount_discovery(&cloud, true);
        cloud.mock(|when, then| {
            when.method(POST).path("/api/v1/uploads");
            then.status(200).json_body(json!({
                "clip_id": "c1",
                "upload_id": "u1",
                "mode": "single_put",
                "part_size_bytes": bytes.len(),
                "single_put_url": "/api/v1/uploads/u1/content",
                "parts_url_template": null
            }));
        });
        mount_progress(&cloud, "u1", "c1", "uploading", bytes.len() as u64, vec![]);
        let single_put = cloud.mock(|when, then| {
            when.method(PUT)
                .path("/api/v1/uploads/u1/content")
                .header("content-type", "video/mp4")
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

        let client = test_client(&cloud);
        let progress = upload_mp4_bytes_with_progress(
            &client,
            TOKEN,
            &upload_request(bytes),
            None,
            bytes,
            |_| {},
        )
        .await
        .expect("upload");

        assert_eq!(progress.status, "completed");
        single_put.assert();
    }

    #[tokio::test]
    async fn direct_s3_chunked_upload_presigns_puts_acks_and_completes() {
        let bytes = b"abcdef";
        let cloud = MockServer::start();
        let s3 = MockServer::start();
        mount_discovery(&cloud, true);
        mount_chunked_create(
            &cloud,
            "u1",
            "c1",
            Some("/api/v1/uploads/u1/parts/{part_number}/direct-presign"),
            Some("/api/v1/uploads/u1/parts/{part_number}/direct-ack"),
        );
        mount_progress(
            &cloud,
            "u1",
            "c1",
            "uploading",
            bytes.len() as u64,
            vec![1, 2],
        );
        let presign1 = mount_presign(&cloud, &s3, "u1", 1, 3, "/s3-part-1", "abc");
        let presign2 = mount_presign(&cloud, &s3, "u1", 2, 3, "/s3-part-2", "def");
        let put1 = mount_s3_put(&s3, "/s3-part-1", "abc", "\"etag-1\"", 200);
        let put2 = mount_s3_put(&s3, "/s3-part-2", "def", "\"etag-2\"", 200);
        let ack1 = mount_ack(&cloud, "u1", 1, "\"etag-1\"", "abc", 200);
        let ack2 = mount_ack(&cloud, "u1", 2, "\"etag-2\"", "def", 200);
        let complete = mount_complete(&cloud, "u1", "c1", bytes.len() as u64);

        let client = test_client(&cloud);
        let progress = upload_mp4_bytes_with_progress(
            &client,
            TOKEN,
            &upload_request(bytes),
            None,
            bytes,
            |_| {},
        )
        .await
        .expect("upload");

        assert_eq!(progress.status, "completed");
        for mock in [presign1, presign2, put1, put2, ack1, ack2, complete] {
            mock.assert();
        }
    }

    #[tokio::test]
    async fn direct_s3_put_expiry_requests_fresh_presign_for_same_part() {
        let bytes = b"abc";
        let cloud = MockServer::start();
        let s3 = MockServer::start();
        mount_discovery(&cloud, true);
        mount_chunked_create(
            &cloud,
            "u1",
            "c1",
            Some("/api/v1/uploads/u1/parts/{part_number}/direct-presign"),
            Some("/api/v1/uploads/u1/parts/{part_number}/direct-ack"),
        );
        mount_progress(&cloud, "u1", "c1", "uploading", bytes.len() as u64, vec![1]);
        let presign = mount_presign(&cloud, &s3, "u1", 1, 3, "/expired-part-1", "abc");
        let expired_put = mount_s3_put(&s3, "/expired-part-1", "abc", "\"expired\"", 403);

        let client = test_client(&cloud);
        let error = upload_mp4_bytes_with_progress(
            &client,
            TOKEN,
            &upload_request(bytes),
            None,
            bytes,
            |_| {},
        )
        .await
        .expect_err("expired presign");

        assert!(
            error
                .to_string()
                .contains("failed after refreshing presign"),
            "{error}"
        );
        presign.assert_hits(DIRECT_PUT_MAX_ATTEMPTS);
        expired_put.assert_hits(DIRECT_PUT_MAX_ATTEMPTS);
    }

    #[tokio::test]
    async fn missing_direct_s3_etag_fails_with_retryable_upload_guidance() {
        let bytes = b"abc";
        let cloud = MockServer::start();
        let s3 = MockServer::start();
        mount_discovery(&cloud, true);
        mount_chunked_create(
            &cloud,
            "u1",
            "c1",
            Some("/api/v1/uploads/u1/parts/{part_number}/direct-presign"),
            Some("/api/v1/uploads/u1/parts/{part_number}/direct-ack"),
        );
        mount_progress(&cloud, "u1", "c1", "uploading", bytes.len() as u64, vec![1]);
        mount_presign(&cloud, &s3, "u1", 1, 3, "/s3-part-1", "abc");
        s3.mock(|when, then| {
            when.method(PUT).path("/s3-part-1").body("abc");
            then.status(200);
        });

        let client = test_client(&cloud);
        let error = upload_mp4_bytes_with_progress(
            &client,
            TOKEN,
            &upload_request(bytes),
            None,
            bytes,
            |_| {},
        )
        .await
        .expect_err("etag missing");

        assert!(
            error
                .to_string()
                .contains("direct S3 upload did not return an ETag"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn direct_s3_ack_conflict_surfaces_clear_retry_guidance() {
        let bytes = b"abc";
        let cloud = MockServer::start();
        let s3 = MockServer::start();
        mount_discovery(&cloud, true);
        mount_chunked_create(
            &cloud,
            "u1",
            "c1",
            Some("/api/v1/uploads/u1/parts/{part_number}/direct-presign"),
            Some("/api/v1/uploads/u1/parts/{part_number}/direct-ack"),
        );
        mount_progress(&cloud, "u1", "c1", "uploading", bytes.len() as u64, vec![1]);
        mount_presign(&cloud, &s3, "u1", 1, 3, "/s3-part-1", "abc");
        mount_s3_put(&s3, "/s3-part-1", "abc", "\"etag-1\"", 200);
        mount_ack(&cloud, "u1", 1, "\"etag-1\"", "abc", 409);

        let client = test_client(&cloud);
        let error = upload_mp4_bytes_with_progress(
            &client,
            TOKEN,
            &upload_request(bytes),
            None,
            bytes,
            |_| {},
        )
        .await
        .expect_err("ack conflict");

        assert!(error.to_string().contains("Retry the upload"), "{error}");
    }

    #[tokio::test]
    async fn direct_s3_template_missing_falls_back_to_proxy_parts() {
        let bytes = b"abcdef";
        let cloud = MockServer::start();
        mount_discovery(&cloud, true);
        mount_chunked_create(&cloud, "u1", "c1", None, None);
        mount_progress(
            &cloud,
            "u1",
            "c1",
            "uploading",
            bytes.len() as u64,
            vec![1, 2],
        );
        let part1 = mount_proxy_part(&cloud, "u1", 1, "abc");
        let part2 = mount_proxy_part(&cloud, "u1", 2, "def");
        mount_complete(&cloud, "u1", "c1", bytes.len() as u64);

        let client = test_client(&cloud);
        let progress = upload_mp4_bytes_with_progress(
            &client,
            TOKEN,
            &upload_request(bytes),
            None,
            bytes,
            |_| {},
        )
        .await
        .expect("upload");

        assert_eq!(progress.status, "completed");
        part1.assert();
        part2.assert();
    }

    #[tokio::test]
    async fn direct_s3_provider_failure_restarts_with_proxy_upload() {
        let bytes = b"abcdef";
        let cloud = MockServer::start();
        let s3 = MockServer::start();
        mount_discovery(&cloud, true);
        let create = cloud.mock(|when, then| {
            when.method(POST)
                .path("/api/v1/uploads")
                .json_body_partial(r#"{"description":"Retry context"}"#);
            then.status(200).json_body(json!({
                "clip_id": "c1",
                "upload_id": "u1",
                "mode": "chunked",
                "part_size_bytes": 3,
                "single_put_url": null,
                "parts_url_template": "/api/v1/uploads/u1/parts/{part_number}",
                "direct_part_presign_url_template": "/api/v1/uploads/u1/parts/{part_number}/direct-presign",
                "direct_part_ack_url_template": "/api/v1/uploads/u1/parts/{part_number}/direct-ack"
            }));
        });
        mount_progress(
            &cloud,
            "u1",
            "c1",
            "uploading",
            bytes.len() as u64,
            vec![1, 2],
        );
        mount_presign(&cloud, &s3, "u1", 1, 3, "/provider-fail", "abc");
        let failed_put = mount_s3_put(&s3, "/provider-fail", "abc", "\"bad\"", 400);
        let proxy_part1 = mount_proxy_part(&cloud, "u1", 1, "abc");
        let proxy_part2 = mount_proxy_part(&cloud, "u1", 2, "def");
        let complete = mount_complete(&cloud, "u1", "c1", bytes.len() as u64);

        let client = test_client(&cloud);
        let progress = upload_mp4_bytes_with_progress(
            &client,
            TOKEN,
            &upload_request(bytes),
            Some("  Retry context  "),
            bytes,
            |_| {},
        )
        .await
        .expect("fallback upload");

        assert_eq!(progress.clip_id, "c1");
        create.assert_hits(2);
        failed_put.assert();
        proxy_part1.assert();
        proxy_part2.assert();
        complete.assert();
    }

}