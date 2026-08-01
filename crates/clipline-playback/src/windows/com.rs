use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Media::MediaFoundation::{MFShutdown, MFStartup, MFSTARTUP_FULL, MF_VERSION};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows_core::{Error, Result};

/// One balanced COM initialization for the playback worker thread.
///
/// The `Rc` marker deliberately keeps the guard on the thread whose apartment
/// it initialized. Windows COM interface wrappers may move independently when
/// their contracts allow it; the apartment guard may not.
pub(crate) struct ComApartment {
    _thread_affinity: PhantomData<Rc<()>>,
}

impl ComApartment {
    pub(crate) fn multithreaded() -> Result<Self> {
        // SAFETY: initializes COM for the current thread. A successful S_OK or
        // S_FALSE result owns exactly one matching CoUninitialize below.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
        Ok(Self {
            _thread_affinity: PhantomData,
        })
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: this non-Send guard drops on the thread where initialization
        // succeeded and balances that successful CoInitializeEx call.
        unsafe { CoUninitialize() };
    }
}

#[derive(Default)]
struct MediaFoundationState {
    playback_references: usize,
}

fn media_foundation_state() -> &'static Mutex<MediaFoundationState> {
    static STATE: OnceLock<Mutex<MediaFoundationState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(MediaFoundationState::default()))
}

/// A process-wide playback-owned Media Foundation reference.
///
/// Reopening clips increments this Rust-side count without repeatedly calling
/// MFStartup. The final playback guard balances the one process-wide startup.
pub(crate) struct MediaFoundationRuntime;

impl MediaFoundationRuntime {
    pub(crate) fn acquire() -> Result<Self> {
        let mut state = media_foundation_state()
            .lock()
            .map_err(|_| Error::new(E_FAIL, "Media Foundation runtime lock poisoned"))?;
        if state.playback_references == 0 {
            // SAFETY: process initialization is serialized by the mutex and is
            // balanced by MFShutdown when the final playback reference drops.
            unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL)? };
        }
        state.playback_references = state
            .playback_references
            .checked_add(1)
            .ok_or_else(|| Error::new(E_FAIL, "Media Foundation reference count overflow"))?;
        Ok(Self)
    }
}

impl Drop for MediaFoundationRuntime {
    fn drop(&mut self) {
        let Ok(mut state) = media_foundation_state().lock() else {
            return;
        };
        debug_assert!(state.playback_references != 0);
        if state.playback_references == 0 {
            return;
        }
        state.playback_references -= 1;
        if state.playback_references == 0 {
            // SAFETY: balances the single MFStartup owned by this playback
            // reference group. Other subsystems retain their own MFStartup.
            let _ = unsafe { MFShutdown() };
        }
    }
}
