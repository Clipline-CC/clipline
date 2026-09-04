//! Upload transport, template, and error types.
use super::*;

#[derive(Clone, Copy)]
pub(crate) struct UploadTransport<'a> {
    pub(crate) client: &'a CloudClient,
    pub(crate) authenticated_control: &'a reqwest::Client,
    pub(crate) authenticated_stream: &'a reqwest::Client,
    pub(crate) object_http: &'a reqwest::Client,
    pub(crate) device_token: &'a str,
}

#[derive(Clone, Copy)]
pub(crate) struct DirectPartTemplates<'a> {
    pub(crate) presign: &'a str,
    pub(crate) ack: &'a str,
}

#[derive(Debug)]
pub(crate) enum DirectUploadError {
    Fallback(String),
    Cloud(CloudApiError),
}

impl DirectUploadError {
    pub(crate) fn into_cloud_error(self) -> CloudApiError {
        match self {
            Self::Fallback(message) => CloudApiError::InvalidUpload(message),
            Self::Cloud(error) => error,
        }
    }
}

#[derive(Debug)]
pub(crate) enum DirectPutError {
    Retryable {
        message: String,
        retry_after: Option<Duration>,
    },
    Fallback(String),
    Terminal(CloudApiError),
}

#[derive(Debug, Deserialize)]
pub(crate) struct ErrorResponse {
    pub(crate) error: String,
}
