//! Production Cloud readiness, visibility, and media-verification port.

use std::time::Duration;

use crate::http::ReqwestCloudProtocol;
use crate::protocol::{ClipDetailResponse, CloudProtocolError, UpdateVisibilityRequest};
use crate::{
    ReadyUpload, UploadCancellation, UploadEndpoint, UploadFuture, UploadRemoteOutcome,
    UploadRemotePort, UploadWorkError,
};

const READY_POLL_ATTEMPTS: usize = 30;
const READY_POLL_DELAY: Duration = Duration::from_secs(1);

/// Redirect-free bounded HTTP implementation of the upload post-processing
/// boundary. Every request and delay races explicit durable-job cancellation.
#[derive(Debug, Clone)]
pub struct ReqwestUploadRemote {
    attempts: usize,
    delay: Duration,
}

impl Default for ReqwestUploadRemote {
    fn default() -> Self {
        Self {
            attempts: READY_POLL_ATTEMPTS,
            delay: READY_POLL_DELAY,
        }
    }
}

impl ReqwestUploadRemote {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn with_policy(attempts: usize, delay: Duration) -> Self {
        assert!(attempts > 0);
        Self { attempts, delay }
    }
}

impl UploadRemotePort for ReqwestUploadRemote {
    fn wait_until_ready<'a>(
        &'a self,
        endpoint: &'a UploadEndpoint,
        remote_clip_id: &'a str,
        visibility: &'a str,
        cancellation: &'a UploadCancellation,
    ) -> UploadFuture<'a, Result<UploadRemoteOutcome, UploadWorkError>> {
        Box::pin(async move {
            cancellation.check().map_err(UploadWorkError::from)?;
            let client = ReqwestCloudProtocol::new(endpoint.api().clone())?;
            for attempt in 0..self.attempts {
                let clip = match cancel_protocol_request(
                    cancellation,
                    client.get_clip(endpoint.credential().expose(), remote_clip_id),
                )
                .await
                {
                    Ok(clip) => Some(clip),
                    Err(error) if error.is_not_found() => None,
                    Err(error) => return Err(error.into()),
                };

                if let Some(clip) = clip {
                    match clip.status.as_str() {
                        "ready" => {
                            let clip = reconcile_visibility(
                                &client,
                                endpoint,
                                clip,
                                visibility,
                                cancellation,
                            )
                            .await?;
                            return Ok(UploadRemoteOutcome::Ready(ReadyUpload {
                                remote_clip_id: clip.id,
                                visibility: clip.visibility.clone(),
                                remote_url: if clip.visibility == "private" {
                                    None
                                } else {
                                    clip.public_url
                                },
                            }));
                        }
                        "failed" => {
                            return Ok(UploadRemoteOutcome::ProcessingFailed(
                                "cloud media processing failed".into(),
                            ));
                        }
                        "created" | "uploading" | "processing" => {}
                        other => {
                            return Err(UploadWorkError::failed(format!(
                                "cloud clip returned unsupported processing status {other:?}"
                            )));
                        }
                    }
                }

                if attempt + 1 < self.attempts {
                    cancel_delay(cancellation, self.delay).await?;
                }
            }
            Ok(UploadRemoteOutcome::ProcessingTimedOut)
        })
    }

    fn probe_media<'a>(
        &'a self,
        endpoint: &'a UploadEndpoint,
        remote_clip_id: &'a str,
        cancellation: &'a UploadCancellation,
    ) -> UploadFuture<'a, Result<(), UploadWorkError>> {
        Box::pin(async move {
            cancellation.check().map_err(UploadWorkError::from)?;
            let client = ReqwestCloudProtocol::new(endpoint.api().clone())?;
            cancel_protocol_request(
                cancellation,
                client.probe_media(endpoint.credential().expose(), remote_clip_id),
            )
            .await
            .map_err(UploadWorkError::from)
            .map(|_| ())
        })
    }
}

async fn reconcile_visibility(
    client: &ReqwestCloudProtocol,
    endpoint: &UploadEndpoint,
    clip: ClipDetailResponse,
    visibility: &str,
    cancellation: &UploadCancellation,
) -> Result<ClipDetailResponse, UploadWorkError> {
    if clip.visibility == visibility {
        return Ok(clip);
    }
    let updated = cancel_protocol_request(
        cancellation,
        client.update_visibility(
            endpoint.credential().expose(),
            &clip.id,
            &UpdateVisibilityRequest {
                visibility: visibility.to_owned(),
            },
        ),
    )
    .await
    .map_err(UploadWorkError::from)?;
    match cancel_protocol_request(
        cancellation,
        client.get_clip(endpoint.credential().expose(), &clip.id),
    )
    .await
    {
        Ok(refreshed) => Ok(refreshed),
        Err(_) if updated.visibility == "private" || updated.public_url.is_some() => Ok(updated),
        Err(error) => Err(UploadWorkError::failed(format!(
            "visibility changed, but refreshing the canonical public URL failed: {error}"
        ))),
    }
}

async fn cancel_protocol_request<T>(
    cancellation: &UploadCancellation,
    request: impl std::future::Future<Output = Result<T, CloudProtocolError>>,
) -> Result<T, CloudProtocolError> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(CloudProtocolError::Canceled),
        result = request => result,
    }
}

async fn cancel_delay(
    cancellation: &UploadCancellation,
    delay: Duration,
) -> Result<(), UploadWorkError> {
    if delay.is_zero() {
        cancellation.check().map_err(UploadWorkError::from)
    } else {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(UploadWorkError::Canceled),
            _ = tokio::time::sleep(delay) => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::CloudCredential;
    use crate::protocol::CloudApiBase;
    use crate::{CloudAccountGeneration, CloudAccountKey, UploadAccountOwner};
    use httpmock::prelude::*;

    fn clip_json(status: &str, visibility: &str) -> serde_json::Value {
        serde_json::json!({
            "id": "clip-1",
            "client_clip_id": "client-1",
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
            "status": status,
            "public_share_id": null,
            "public_url": if visibility == "private" { None } else { Some("https://clips.example/c/1") },
            "view_count": 0,
            "markers": [],
            "created_at": "2026-08-02T00:00:00Z",
            "updated_at": "2026-08-02T00:00:00Z"
        })
    }

    fn endpoint(server: &MockServer) -> UploadEndpoint {
        UploadEndpoint::new(
            UploadAccountOwner::new(
                CloudAccountKey::new("account-a").unwrap(),
                CloudAccountGeneration::new(1),
            ),
            CloudApiBase::parse(&format!("{}/", server.base_url()), true).unwrap(),
            CloudCredential::new("device-secret"),
        )
    }

    #[tokio::test]
    async fn ready_and_media_probe_preserve_auth_and_typed_result() {
        let server = MockServer::start();
        let ready = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/clips/clip-1")
                .header("authorization", "Bearer device-secret");
            then.status(200).json_body(clip_json("ready", "private"));
        });
        let media = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/clips/clip-1/media")
                .header("authorization", "Bearer device-secret")
                .header("range", "bytes=0-0");
            then.status(206).body(vec![0]);
        });
        let remote = ReqwestUploadRemote::with_policy(1, Duration::ZERO);
        let endpoint = endpoint(&server);
        let cancellation = UploadCancellation::default();

        let result = remote
            .wait_until_ready(&endpoint, "clip-1", "private", &cancellation)
            .await
            .unwrap();
        assert!(matches!(
            result,
            UploadRemoteOutcome::Ready(ReadyUpload {
                ref remote_clip_id,
                ref visibility,
                remote_url: None,
            }) if remote_clip_id == "clip-1" && visibility == "private"
        ));
        remote
            .probe_media(&endpoint, "clip-1", &cancellation)
            .await
            .unwrap();
        ready.assert_hits(1);
        media.assert_hits(1);
    }

    #[tokio::test]
    async fn processing_timeout_and_cancellation_are_distinct() {
        let server = MockServer::start();
        let processing = server.mock(|when, then| {
            when.method(GET).path("/api/v1/clips/clip-1");
            then.status(200)
                .json_body(clip_json("processing", "private"));
        });
        let endpoint = endpoint(&server);
        let remote = ReqwestUploadRemote::with_policy(2, Duration::ZERO);
        assert_eq!(
            remote
                .wait_until_ready(
                    &endpoint,
                    "clip-1",
                    "private",
                    &UploadCancellation::default(),
                )
                .await
                .unwrap(),
            UploadRemoteOutcome::ProcessingTimedOut
        );
        processing.assert_hits(2);

        let canceled = UploadCancellation::default();
        canceled.cancel();
        assert!(matches!(
            remote
                .wait_until_ready(&endpoint, "clip-1", "private", &canceled)
                .await,
            Err(UploadWorkError::Canceled)
        ));
        processing.assert_hits(2);
    }

    #[tokio::test]
    async fn unsupported_remote_status_fails_closed_without_polling_again() {
        let server = MockServer::start();
        let invalid = server.mock(|when, then| {
            when.method(GET).path("/api/v1/clips/clip-1");
            then.status(200)
                .json_body(clip_json("unexpected", "private"));
        });
        let endpoint = endpoint(&server);
        let remote = ReqwestUploadRemote::with_policy(3, Duration::ZERO);

        let error = remote
            .wait_until_ready(
                &endpoint,
                "clip-1",
                "private",
                &UploadCancellation::default(),
            )
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            UploadWorkError::Failed(message) if message.contains("unsupported processing status")
        ));
        invalid.assert_hits(1);
    }
}
