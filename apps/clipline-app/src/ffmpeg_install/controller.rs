//! Single-flight install controller: queryable across UI destroy/recreate.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use super::FfmpegInstallState;
#[derive(Debug, Default)]
struct InstallInner {
    state: FfmpegInstallState,
    job_active: bool,
}

/// Process-global install controller: queryable across UI destroy/recreate.
pub struct FfmpegInstallController {
    inner: Mutex<InstallInner>,
    cancel: AtomicBool,
}

impl Default for FfmpegInstallController {
    fn default() -> Self {
        Self {
            inner: Mutex::new(InstallInner::default()),
            cancel: AtomicBool::new(false),
        }
    }
}

impl FfmpegInstallController {
    pub fn snapshot_state(&self) -> FfmpegInstallState {
        self.inner
            .lock()
            .map(|guard| guard.state.clone())
            .unwrap_or(FfmpegInstallState::Failed {
                message: "ffmpeg install state lock poisoned".into(),
            })
    }

    pub fn set_state(&self, state: FfmpegInstallState) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.state = state;
        }
    }

    pub fn request_cancel(&self) -> FfmpegInstallState {
        let guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.job_active && !matches!(guard.state, FfmpegInstallState::Publishing) {
            self.cancel.store(true, Ordering::Release);
        }
        guard.state.clone()
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }

    /// Atomically cross the last cancellable boundary. Once this succeeds,
    /// cancellation is ignored until the publish operation completes.
    pub fn begin_publishing(&self) -> Result<bool, String> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| "ffmpeg install state lock poisoned".to_string())?;
        if !guard.job_active || self.cancel.load(Ordering::Acquire) {
            return Ok(false);
        }
        guard.state = FfmpegInstallState::Publishing;
        Ok(true)
    }

    /// Returns true when this caller should start the job; false when coalesced.
    pub fn try_begin_job(&self) -> Result<bool, String> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| "ffmpeg install state lock poisoned".to_string())?;
        if guard.job_active {
            return Ok(false);
        }
        match &guard.state {
            FfmpegInstallState::Checking
            | FfmpegInstallState::Downloading { .. }
            | FfmpegInstallState::Verifying
            | FfmpegInstallState::Publishing => return Ok(false),
            FfmpegInstallState::Ready
            | FfmpegInstallState::Idle
            | FfmpegInstallState::Failed { .. }
            | FfmpegInstallState::Cancelled => {}
        }
        guard.job_active = true;
        guard.state = FfmpegInstallState::Checking;
        self.cancel.store(false, Ordering::Release);
        Ok(true)
    }

    pub fn end_job(&self, state: FfmpegInstallState) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.job_active = false;
            guard.state = state;
        }
    }
}
