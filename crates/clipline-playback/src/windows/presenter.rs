use std::cell::Cell;
use std::ffi::c_void;
use std::marker::PhantomData;
use std::num::NonZeroIsize;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use thiserror::Error;
use windows::Win32::Foundation::{DXGI_STATUS_OCCLUDED, HWND, S_OK};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_DEVICE_HUNG, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
    DXGI_ERROR_DRIVER_INTERNAL_ERROR, DXGI_ERROR_WAS_STILL_DRAWING,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, GetWindowThreadProcessId, IsWindow, SetWindowPos, ShowWindow,
    SWP_NOACTIVATE, SWP_NOZORDER, SWP_SHOWWINDOW, SW_HIDE, WS_CHILD, WS_CLIPCHILDREN,
    WS_CLIPSIBLINGS, WS_EX_NOACTIVATE,
};
use windows_core::w;

use crate::{
    PhysicalVideoRect, PresentationError, PresentationLifecycle, PresentationState,
    PresentationUpdate,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentOutcome {
    Presented,
    Occluded,
    Backpressured,
    DeviceLost,
    Failed,
}

pub fn classify_present_result(native_code: i32) -> PresentOutcome {
    match native_code {
        code if code == S_OK.0 => PresentOutcome::Presented,
        code if code == DXGI_STATUS_OCCLUDED.0 => PresentOutcome::Occluded,
        code if code == DXGI_ERROR_WAS_STILL_DRAWING.0 => PresentOutcome::Backpressured,
        code if [
            DXGI_ERROR_DEVICE_REMOVED.0,
            DXGI_ERROR_DEVICE_RESET.0,
            DXGI_ERROR_DEVICE_HUNG.0,
            DXGI_ERROR_DRIVER_INTERNAL_ERROR.0,
        ]
        .contains(&code) =>
        {
            PresentOutcome::DeviceLost
        }
        _ => PresentOutcome::Failed,
    }
}

#[derive(Debug, Clone, PartialEq, Error)]
pub enum VideoHostError {
    #[error("parent HWND must be non-zero")]
    NullParent,
    #[error("parent HWND is not a live window")]
    InvalidParent,
    #[error("parent HWND belongs to thread {expected}, not the current thread {actual}")]
    ParentThreadMismatch { expected: u32, actual: u32 },
    #[error("video-stage bounds exceed the Win32 coordinate range")]
    InvalidBounds,
    #[error("video host may only be accessed from its creating thread")]
    WrongThread,
    #[error("video host is closed")]
    Closed,
    #[error("video host already has an attached publisher target")]
    TargetAlreadyAttached,
    #[error("video publisher target must be dropped before the host closes")]
    PublisherStillAttached,
    #[error("video target state lock is poisoned")]
    StatePoisoned,
    #[error("video presentation state failed: {0}")]
    Presentation(#[from] PresentationError),
    #[error("Win32 video host operation failed with HRESULT 0x{native_code:08X}: {message}")]
    Native { native_code: u32, message: String },
}

#[derive(Debug)]
struct VideoTargetState {
    snapshot: Mutex<VideoTargetSnapshot>,
    publisher_attached: AtomicBool,
}

#[derive(Debug, Clone, Copy)]
struct VideoTargetSnapshot {
    revision: u64,
    presentable: bool,
    alive: bool,
}

/// Move-only playback-thread lease for the child window. Dropping it permits
/// the UI-thread host to destroy the HWND.
#[derive(Debug)]
pub struct WindowsVideoTarget {
    raw_hwnd: NonZeroIsize,
    state: Arc<VideoTargetState>,
    _not_sync: PhantomData<Cell<()>>,
}

impl Drop for WindowsVideoTarget {
    fn drop(&mut self) {
        self.state
            .publisher_attached
            .store(false, Ordering::Release);
    }
}

impl WindowsVideoTarget {
    pub const fn raw_handle(&self) -> NonZeroIsize {
        self.raw_hwnd
    }

    pub fn latest_revision(&self) -> u64 {
        self.snapshot().revision
    }

    pub fn is_presentable(&self) -> bool {
        let snapshot = self.snapshot();
        snapshot.alive && snapshot.presentable
    }

    pub fn is_alive(&self) -> bool {
        self.snapshot().alive
    }

    fn snapshot(&self) -> VideoTargetSnapshot {
        self.state
            .snapshot
            .lock()
            .map(|snapshot| *snapshot)
            .unwrap_or(VideoTargetSnapshot {
                revision: u64::MAX,
                presentable: false,
                alive: false,
            })
    }
}

/// UI-thread owner of Clipline's non-activating video child window.
///
/// The `Rc` marker intentionally makes this type neither `Send` nor `Sync`.
pub struct WindowsVideoHost {
    hwnd: Option<HWND>,
    owner_thread: u32,
    lifecycle: PresentationLifecycle,
    target_state: Arc<VideoTargetState>,
    _thread_affinity: PhantomData<Rc<()>>,
}

impl std::fmt::Debug for WindowsVideoHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WindowsVideoHost")
            .field("raw_handle", &self.raw_handle())
            .field("owner_thread", &self.owner_thread)
            .field("revision", &self.lifecycle.latest_revision())
            .field("presentable", &self.lifecycle.is_presentable())
            .finish()
    }
}

impl WindowsVideoHost {
    pub fn attach(parent_raw_hwnd: isize) -> Result<Self, VideoHostError> {
        let parent_raw = NonZeroIsize::new(parent_raw_hwnd).ok_or(VideoHostError::NullParent)?;
        let parent = hwnd(parent_raw);
        // SAFETY: `IsWindow` only validates the opaque value and does not
        // dereference caller memory.
        if !unsafe { IsWindow(Some(parent)) }.as_bool() {
            return Err(VideoHostError::InvalidParent);
        }
        // SAFETY: the live parent was validated above and the process-id
        // output is deliberately omitted.
        let expected = unsafe { GetWindowThreadProcessId(parent, None) };
        // SAFETY: this pure query has no preconditions.
        let actual = unsafe { GetCurrentThreadId() };
        if expected != actual {
            return Err(VideoHostError::ParentThreadMismatch { expected, actual });
        }

        // SAFETY: the built-in STATIC class is process-independent, the live
        // parent was validated above, and all optional pointer parameters are
        // null. The child starts hidden until valid geometry is committed.
        let child = unsafe {
            CreateWindowExW(
                WS_EX_NOACTIVATE,
                w!("STATIC"),
                w!(""),
                WS_CHILD | WS_CLIPSIBLINGS | WS_CLIPCHILDREN,
                0,
                0,
                1,
                1,
                Some(parent),
                None,
                None,
                None,
            )
        }
        .map_err(|error| native_error(error, "create video child window"))?;
        NonZeroIsize::new(child.0 as isize).ok_or_else(|| VideoHostError::Native {
            native_code: 0,
            message: "CreateWindowExW returned a null video child".into(),
        })?;
        let target_state = Arc::new(VideoTargetState {
            snapshot: Mutex::new(VideoTargetSnapshot {
                revision: 0,
                presentable: false,
                alive: true,
            }),
            publisher_attached: AtomicBool::new(false),
        });
        Ok(Self {
            hwnd: Some(child),
            // SAFETY: this pure query has no preconditions.
            owner_thread: unsafe { GetCurrentThreadId() },
            lifecycle: PresentationLifecycle::new(),
            target_state,
            _thread_affinity: PhantomData,
        })
    }

    pub fn take_target(&mut self) -> Result<WindowsVideoTarget, VideoHostError> {
        let hwnd = self.hwnd.ok_or(VideoHostError::Closed)?;
        self.target_state
            .publisher_attached
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| VideoHostError::TargetAlreadyAttached)?;
        let raw_hwnd = NonZeroIsize::new(hwnd.0 as isize).ok_or(VideoHostError::Closed)?;
        Ok(WindowsVideoTarget {
            raw_hwnd,
            state: Arc::clone(&self.target_state),
            _not_sync: PhantomData,
        })
    }

    pub fn update(
        &mut self,
        bounds: PhysicalVideoRect,
        state: PresentationState,
    ) -> Result<PresentationUpdate, VideoHostError> {
        self.require_owner_thread()?;
        let hwnd = self.hwnd.ok_or(VideoHostError::Closed)?;
        validate_video_bounds(bounds)?;

        let mut candidate = self.lifecycle.clone();
        let update = candidate.update(bounds, state)?;
        if matches!(update, PresentationUpdate::Unchanged { .. }) {
            return Ok(update);
        }

        let revision = match update {
            PresentationUpdate::Changed { revision, .. } => revision,
            PresentationUpdate::Unchanged { .. } => unreachable!(),
        };
        {
            let mut snapshot = self
                .target_state
                .snapshot
                .lock()
                .map_err(|_| VideoHostError::StatePoisoned)?;
            snapshot.revision = revision;
            snapshot.presentable = false;
        }

        if state == PresentationState::Visible && bounds.has_area() {
            // SAFETY: `hwnd` is owned by this host and dimensions were checked
            // against the Win32 signed coordinate range above.
            unsafe {
                SetWindowPos(
                    hwnd,
                    None,
                    bounds.x,
                    bounds.y,
                    bounds.width as i32,
                    bounds.height as i32,
                    SWP_NOACTIVATE | SWP_NOZORDER | SWP_SHOWWINDOW,
                )
            }
            .map_err(|error| native_error(error, "position video child window"))?;
        } else {
            // SAFETY: hiding an owned live window has no pointer preconditions.
            let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
        }

        self.lifecycle = candidate;
        let mut snapshot = self
            .target_state
            .snapshot
            .lock()
            .map_err(|_| VideoHostError::StatePoisoned)?;
        snapshot.presentable = self.lifecycle.is_presentable();
        Ok(update)
    }

    pub fn latest_revision(&self) -> u64 {
        self.lifecycle.latest_revision()
    }

    pub fn is_presentable(&self) -> bool {
        self.lifecycle.is_presentable()
    }

    pub fn raw_handle(&self) -> Option<NonZeroIsize> {
        self.hwnd
            .and_then(|hwnd| NonZeroIsize::new(hwnd.0 as isize))
    }

    pub fn close(&mut self) -> Result<(), VideoHostError> {
        self.require_owner_thread()?;
        let Some(hwnd) = self.hwnd.take() else {
            return Ok(());
        };
        if self.target_state.publisher_attached.load(Ordering::Acquire) {
            self.hwnd = Some(hwnd);
            return Err(VideoHostError::PublisherStillAttached);
        }
        let mut snapshot = self
            .target_state
            .snapshot
            .lock()
            .map_err(|_| VideoHostError::StatePoisoned)?;
        snapshot.presentable = false;
        snapshot.alive = false;
        drop(snapshot);
        // SAFETY: `hwnd` is the live child uniquely owned by this host.
        unsafe { DestroyWindow(hwnd) }
            .map_err(|error| native_error(error, "destroy video child window"))
    }

    fn require_owner_thread(&self) -> Result<(), VideoHostError> {
        // SAFETY: this pure query has no preconditions.
        if unsafe { GetCurrentThreadId() } != self.owner_thread {
            return Err(VideoHostError::WrongThread);
        }
        Ok(())
    }
}

impl Drop for WindowsVideoHost {
    fn drop(&mut self) {
        if self.target_state.publisher_attached.load(Ordering::Acquire) {
            if let Ok(mut snapshot) = self.target_state.snapshot.lock() {
                snapshot.presentable = false;
                snapshot.alive = false;
            }
            if let Some(hwnd) = self.hwnd.take() {
                // SAFETY: hiding the still-parent-owned child prevents further
                // display while the move-only publisher target drains. The OS
                // destroys it with its parent; destroying it here would race
                // the playback thread and permit HWND-reuse ABA.
                let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
            }
            return;
        }
        let _ = self.close();
    }
}

pub fn validate_video_bounds(bounds: PhysicalVideoRect) -> Result<(), VideoHostError> {
    if bounds.x < 0
        || bounds.y < 0
        || bounds.width > i32::MAX as u32
        || bounds.height > i32::MAX as u32
        || bounds.x.checked_add(bounds.width as i32).is_none()
        || bounds.y.checked_add(bounds.height as i32).is_none()
    {
        return Err(VideoHostError::InvalidBounds);
    }
    Ok(())
}

fn hwnd(raw: NonZeroIsize) -> HWND {
    HWND(raw.get() as *mut c_void)
}

fn native_error(error: windows_core::Error, operation: &str) -> VideoHostError {
    VideoHostError::Native {
        native_code: error.code().0 as u32,
        message: format!("{operation}: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, WS_POPUP};

    struct TestParent(HWND);

    impl TestParent {
        fn new() -> Self {
            // SAFETY: the built-in STATIC class needs no registration and all
            // optional pointer parameters are null.
            let hwnd = unsafe {
                CreateWindowExW(
                    Default::default(),
                    w!("STATIC"),
                    w!("Clipline presenter test parent"),
                    WS_POPUP,
                    0,
                    0,
                    640,
                    480,
                    None,
                    None,
                    None,
                    None,
                )
            }
            .expect("create hidden test parent");
            Self(hwnd)
        }

        fn raw(&self) -> isize {
            self.0 .0 as isize
        }
    }

    impl Drop for TestParent {
        fn drop(&mut self) {
            // SAFETY: the test owns this hidden top-level window.
            let _ = unsafe { DestroyWindow(self.0) };
        }
    }

    #[test]
    fn child_window_tracks_latest_geometry_and_closes_once() {
        let parent = TestParent::new();
        let mut host = WindowsVideoHost::attach(parent.raw()).expect("attach child host");
        let target = host.take_target().expect("take playback target");
        let child_raw = target.raw_handle();
        assert_eq!(
            host.take_target().expect_err("target is a single lease"),
            VideoHostError::TargetAlreadyAttached
        );
        assert!(target.is_alive());
        assert!(!target.is_presentable());

        let bounds = PhysicalVideoRect::new(17, 23, 426, 240);
        assert_eq!(
            host.update(bounds, PresentationState::Visible)
                .expect("show child"),
            PresentationUpdate::Changed {
                revision: 1,
                release_pending_frame: false,
            }
        );
        assert_eq!(target.latest_revision(), 1);
        assert!(target.is_presentable());

        let mut client = windows::Win32::Foundation::RECT::default();
        // SAFETY: target still points to the live child owned by `host`.
        unsafe { GetClientRect(hwnd(target.raw_handle()), &mut client) }
            .expect("query child client size");
        assert_eq!(client.right - client.left, 426);
        assert_eq!(client.bottom - client.top, 240);

        assert_eq!(
            host.update(bounds, PresentationState::Visible)
                .expect("repeat geometry"),
            PresentationUpdate::Unchanged { revision: 1 }
        );
        assert_eq!(
            host.update(bounds, PresentationState::Minimized)
                .expect("minimize child"),
            PresentationUpdate::Changed {
                revision: 2,
                release_pending_frame: true,
            }
        );
        assert!(!target.is_presentable());

        assert_eq!(
            host.close().expect_err("publisher must close first"),
            VideoHostError::PublisherStillAttached
        );
        drop(target);
        host.close().expect("close child");
        host.close().expect("close child twice");
        // SAFETY: checking a stale opaque HWND is permitted.
        assert!(!unsafe { IsWindow(Some(hwnd(child_raw))) }.as_bool());
    }

    #[test]
    fn parent_from_another_thread_is_rejected_before_child_creation() {
        let (handle_tx, handle_rx) = std::sync::mpsc::sync_channel(1);
        let (close_tx, close_rx) = std::sync::mpsc::sync_channel(1);
        let owner = std::thread::spawn(move || {
            let parent = TestParent::new();
            handle_tx.send(parent.raw()).expect("publish test HWND");
            close_rx.recv().expect("wait before destroying parent");
        });
        let parent_raw = handle_rx.recv().expect("receive test HWND");
        assert!(matches!(
            WindowsVideoHost::attach(parent_raw),
            Err(VideoHostError::ParentThreadMismatch { .. })
        ));
        close_tx.send(()).expect("release parent thread");
        owner.join().expect("join parent thread");
    }
}
