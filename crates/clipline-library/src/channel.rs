use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use thiserror::Error;

use crate::{
    CatalogOperationOwner, CatalogResult, ClipDetailOwner, CloudReviewMediaOwner,
    CloudThumbnailOwner, CloudWorkToken, DurableUploadToken, GenerationError, PosterWorkToken,
    WindowWorkToken,
};

pub const CATALOG_RESULT_CAPACITY: usize = 128;
pub const CATALOG_RESULT_BYTE_CAPACITY: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpectedResultOwner {
    Window(WindowWorkToken),
    Detail(ClipDetailOwner),
    Poster(PosterWorkToken),
    Cloud(CloudWorkToken),
    Upload(DurableUploadToken),
    Operation(CatalogOperationOwner),
    CloudReviewMedia(CloudReviewMediaOwner),
    CloudThumbnail(CloudThumbnailOwner),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogResultPublishOutcome {
    Queued,
    Replaced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ResultPortError {
    #[error("catalog result queue is full at capacity {capacity}")]
    Full { capacity: usize },
    #[error("catalog result queue is full at byte capacity {capacity}")]
    ByteCapacity { capacity: usize },
    #[error("{field} contains {actual} bytes or entries; maximum is {maximum}")]
    PayloadTooLarge {
        field: &'static str,
        actual: usize,
        maximum: usize,
    },
    #[error("{field} is invalid")]
    InvalidPayload { field: &'static str },
    #[error("catalog result belongs to stale work")]
    Stale,
    #[error("catalog result belongs to a replaced cloud account")]
    AccountChanged,
    #[error("catalog generation is exhausted")]
    GenerationExhausted,
    #[error("catalog result port is disconnected")]
    Disconnected,
}

#[derive(Debug)]
pub struct RejectedCatalogResult {
    pub error: ResultPortError,
    pub result: CatalogResult,
    pub expected: ExpectedResultOwner,
}

impl From<GenerationError> for ResultPortError {
    fn from(_: GenerationError) -> Self {
        Self::GenerationExhausted
    }
}

impl From<crate::PayloadBoundsError> for ResultPortError {
    fn from(error: crate::PayloadBoundsError) -> Self {
        match error {
            crate::PayloadBoundsError::TooLarge {
                field,
                actual,
                maximum,
            } => Self::PayloadTooLarge {
                field,
                actual,
                maximum,
            },
            crate::PayloadBoundsError::Invalid { field } => Self::InvalidPayload { field },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CoalesceKey {
    LocalIndex(WindowWorkToken),
    ClipDetail(ClipDetailOwner),
    CloudPage(CloudWorkToken),
    Poster(PosterWorkToken),
    CloudThumbnail(CloudThumbnailOwner),
    UploadBytes(DurableUploadToken),
}

fn coalesce_key(result: &CatalogResult) -> Option<CoalesceKey> {
    match result {
        CatalogResult::LocalIndex(completion) => Some(CoalesceKey::LocalIndex(completion.token)),
        CatalogResult::ClipDetail(result) => Some(CoalesceKey::ClipDetail(result.owner().clone())),
        CatalogResult::CloudPage(completion) => {
            Some(CoalesceKey::CloudPage(completion.token.clone()))
        }
        CatalogResult::Poster { token, .. } => Some(CoalesceKey::Poster(token.clone())),
        CatalogResult::CloudThumbnail { owner, .. } => {
            Some(CoalesceKey::CloudThumbnail(owner.clone()))
        }
        CatalogResult::UploadByteProgress { token, .. } => {
            Some(CoalesceKey::UploadBytes(token.clone()))
        }
        // Terminal completions and failures are barriers. They deliberately do
        // not coalesce, so a later result can never replace one across a state
        // transition even when it carries the same exact operation owner.
        CatalogResult::OperationFailed { .. }
        | CatalogResult::CloudReviewMediaPrepared { .. }
        | CatalogResult::RenameCompleted { .. }
        | CatalogResult::DeleteCompleted { .. }
        | CatalogResult::UploadCompleted { .. }
        | CatalogResult::ForegroundFeedback { .. } => None,
    }
}

fn validate_owner(
    result: &CatalogResult,
    expected: &ExpectedResultOwner,
) -> Result<(), ResultPortError> {
    match (result, expected) {
        (
            CatalogResult::LocalIndex(crate::LocalIndexCompletion { token, .. })
            | CatalogResult::RenameCompleted { token, .. }
            | CatalogResult::DeleteCompleted { token, .. }
            | CatalogResult::ForegroundFeedback { token, .. },
            ExpectedResultOwner::Window(expected),
        ) => (*token == *expected)
            .then_some(())
            .ok_or(ResultPortError::Stale),
        (CatalogResult::ClipDetail(result), ExpectedResultOwner::Detail(expected)) => {
            (result.owner() == expected)
                .then_some(())
                .ok_or(ResultPortError::Stale)
        }
        (
            CatalogResult::OperationFailed { owner, .. },
            ExpectedResultOwner::Operation(expected),
        ) => validate_operation_owner(owner, expected),
        (
            CatalogResult::CloudReviewMediaPrepared { owner, .. },
            ExpectedResultOwner::CloudReviewMedia(expected),
        ) => validate_cloud_review_media_owner(owner, expected),
        (
            CatalogResult::CloudThumbnail { owner, .. },
            ExpectedResultOwner::CloudThumbnail(expected),
        ) => validate_cloud_thumbnail_owner(owner, expected),
        (CatalogResult::Poster { token, .. }, ExpectedResultOwner::Poster(expected)) => (token
            == expected)
            .then_some(())
            .ok_or(ResultPortError::Stale),
        (CatalogResult::CloudPage(completion), ExpectedResultOwner::Cloud(expected)) => {
            if completion.token.account_key != expected.account_key
                || completion.token.account_generation != expected.account_generation
            {
                Err(ResultPortError::AccountChanged)
            } else if completion.token.window != expected.window {
                Err(ResultPortError::Stale)
            } else {
                Ok(())
            }
        }
        (
            CatalogResult::UploadByteProgress { token, .. }
            | CatalogResult::UploadCompleted { token, .. },
            ExpectedResultOwner::Upload(expected),
        ) => {
            if token.account_key != expected.account_key
                || token.account_generation != expected.account_generation
            {
                Err(ResultPortError::AccountChanged)
            } else if token != expected {
                Err(ResultPortError::Stale)
            } else {
                Ok(())
            }
        }
        (CatalogResult::CloudPage(_), ExpectedResultOwner::Upload(_))
        | (
            CatalogResult::UploadByteProgress { .. } | CatalogResult::UploadCompleted { .. },
            ExpectedResultOwner::Cloud(_),
        ) => Err(ResultPortError::AccountChanged),
        _ => Err(ResultPortError::Stale),
    }
}

fn validate_operation_owner(
    actual: &CatalogOperationOwner,
    expected: &CatalogOperationOwner,
) -> Result<(), ResultPortError> {
    match (actual.cloud_token(), expected.cloud_token()) {
        (Some(actual), Some(expected))
            if actual.account_key != expected.account_key
                || actual.account_generation != expected.account_generation =>
        {
            Err(ResultPortError::AccountChanged)
        }
        _ if actual == expected => Ok(()),
        _ => Err(ResultPortError::Stale),
    }
}

fn validate_cloud_review_media_owner(
    actual: &CloudReviewMediaOwner,
    expected: &CloudReviewMediaOwner,
) -> Result<(), ResultPortError> {
    if actual.token.account_key != expected.token.account_key
        || actual.token.account_generation != expected.token.account_generation
    {
        Err(ResultPortError::AccountChanged)
    } else if actual == expected {
        Ok(())
    } else {
        Err(ResultPortError::Stale)
    }
}

fn validate_cloud_thumbnail_owner(
    actual: &CloudThumbnailOwner,
    expected: &CloudThumbnailOwner,
) -> Result<(), ResultPortError> {
    if actual.token.account_key != expected.token.account_key
        || actual.token.account_generation != expected.token.account_generation
    {
        Err(ResultPortError::AccountChanged)
    } else if actual == expected {
        Ok(())
    } else {
        Err(ResultPortError::Stale)
    }
}

struct ChannelState {
    queue: VecDeque<CatalogResult>,
    queue_bytes: usize,
    receiver_connected: bool,
    sender_count: usize,
}

struct Shared {
    state: Mutex<ChannelState>,
    ready: Condvar,
}

pub struct CatalogResultSender {
    shared: Arc<Shared>,
}

pub struct CatalogResultReceiver {
    shared: Arc<Shared>,
}

#[must_use]
pub fn catalog_result_channel() -> (CatalogResultSender, CatalogResultReceiver) {
    let shared = Arc::new(Shared {
        state: Mutex::new(ChannelState {
            queue: VecDeque::with_capacity(CATALOG_RESULT_CAPACITY),
            queue_bytes: 0,
            receiver_connected: true,
            sender_count: 1,
        }),
        ready: Condvar::new(),
    });
    (
        CatalogResultSender {
            shared: Arc::clone(&shared),
        },
        CatalogResultReceiver { shared },
    )
}

impl Clone for CatalogResultSender {
    fn clone(&self) -> Self {
        if let Ok(mut state) = self.shared.state.lock() {
            state.sender_count = state.sender_count.saturating_add(1);
        }
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl CatalogResultSender {
    pub fn try_send(
        &self,
        result: CatalogResult,
        expected: ExpectedResultOwner,
    ) -> Result<CatalogResultPublishOutcome, ResultPortError> {
        self.try_send_recoverable(result, expected)
            .map_err(|rejected| rejected.error)
    }

    /// Non-blocking publication that returns ownership of a rejected payload.
    ///
    /// Fixed worker pools use this form to retain one bounded completion while
    /// the UI drains a full result queue. The ordinary `try_send` convenience
    /// remains fail-fast for producers that can recreate or discard progress.
    pub fn try_send_recoverable(
        &self,
        result: CatalogResult,
        expected: ExpectedResultOwner,
    ) -> Result<CatalogResultPublishOutcome, Box<RejectedCatalogResult>> {
        self.try_send_inner(result, expected)
    }

    fn try_send_inner(
        &self,
        result: CatalogResult,
        expected: ExpectedResultOwner,
    ) -> Result<CatalogResultPublishOutcome, Box<RejectedCatalogResult>> {
        macro_rules! reject {
            ($error:expr) => {
                return Err(Box::new(RejectedCatalogResult {
                    error: $error,
                    result,
                    expected,
                }))
            };
        }
        if let Err(error) = validate_owner(&result, &expected) {
            reject!(error);
        }
        if let Err(error) = result.validate_bounds() {
            reject!(ResultPortError::from(error));
        }
        let result_bytes = result.estimated_byte_size();
        let key = coalesce_key(&result);
        let mut state = match self.shared.state.lock() {
            Ok(state) => state,
            Err(_) => reject!(ResultPortError::Disconnected),
        };
        if !state.receiver_connected {
            reject!(ResultPortError::Disconnected);
        }

        let replacement = key.as_ref().and_then(|key| {
            state
                .queue
                .iter()
                .enumerate()
                .rev()
                .take_while(|(_, queued)| !queued.is_barrier())
                .find_map(|(index, queued)| {
                    (coalesce_key(queued).as_ref() == Some(key)).then_some(index)
                })
        });
        if replacement.is_none() && state.queue.len() >= CATALOG_RESULT_CAPACITY {
            reject!(ResultPortError::Full {
                capacity: CATALOG_RESULT_CAPACITY,
            });
        }

        let replaced_bytes = replacement
            .and_then(|index| state.queue.get(index))
            .map_or(0, CatalogResult::estimated_byte_size);
        let projected_bytes = state
            .queue_bytes
            .saturating_sub(replaced_bytes)
            .saturating_add(result_bytes);
        if projected_bytes > CATALOG_RESULT_BYTE_CAPACITY {
            reject!(ResultPortError::ByteCapacity {
                capacity: CATALOG_RESULT_BYTE_CAPACITY,
            });
        }

        let outcome = if let Some(index) = replacement {
            state.queue.remove(index);
            state.queue.push_back(result);
            CatalogResultPublishOutcome::Replaced
        } else {
            state.queue.push_back(result);
            CatalogResultPublishOutcome::Queued
        };
        state.queue_bytes = projected_bytes;
        drop(state);
        self.shared.ready.notify_one();
        Ok(outcome)
    }
}

impl Drop for CatalogResultSender {
    fn drop(&mut self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.sender_count = state.sender_count.saturating_sub(1);
        }
        self.shared.ready.notify_all();
    }
}

impl CatalogResultReceiver {
    #[must_use]
    pub fn try_recv(&self) -> Option<CatalogResult> {
        self.shared.state.lock().ok().and_then(|mut state| {
            let result = state.queue.pop_front()?;
            state.queue_bytes = state
                .queue_bytes
                .saturating_sub(result.estimated_byte_size());
            Some(result)
        })
    }

    pub fn wait_recv(&self, timeout: Duration) -> Result<Option<CatalogResult>, ResultPortError> {
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| ResultPortError::Disconnected)?;
        if state.queue.is_empty() && state.sender_count != 0 {
            state = self
                .shared
                .ready
                .wait_timeout(state, timeout)
                .map_err(|_| ResultPortError::Disconnected)?
                .0;
        }
        if let Some(result) = state.queue.pop_front() {
            state.queue_bytes = state
                .queue_bytes
                .saturating_sub(result.estimated_byte_size());
            Ok(Some(result))
        } else if state.sender_count == 0 {
            Err(ResultPortError::Disconnected)
        } else {
            Ok(None)
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.shared
            .state
            .lock()
            .map_or(0, |state| state.queue.len())
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Drop for CatalogResultReceiver {
    fn drop(&mut self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.receiver_connected = false;
            state.queue.clear();
            state.queue_bytes = 0;
        }
        self.shared.ready.notify_all();
    }
}
