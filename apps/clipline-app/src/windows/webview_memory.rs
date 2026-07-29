//! WebView2 memory-usage target level (ddoc §14: background footprint).
//!
//! `SetIsVisible(false)` stops the view being drawn but leaves most of its
//! memory resident. `MemoryUsageTargetLevel::Low` is the documented lever for
//! shrinking an *inactive* WebView, and unlike `TrySuspend` it keeps scripts and
//! network connections running — the tray-hidden window still handles events and
//! the Save Replay hotkey.
//!
//! Microsoft's guidance is to use `Low`/`Normal` **or** `Suspend`/`Resume`, never
//! mixed. This module only ever sets the target level.

use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2Controller, ICoreWebView2_19, COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL,
    COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW, COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL,
};
use windows_core::Interface;
use windows_core::HRESULT;

/// `E_NOINTERFACE` — the runtime predates `ICoreWebView2_19`.
const E_NOINTERFACE: HRESULT = HRESULT(0x8000_4002u32 as i32);

/// Which target level to request for a webview.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MemoryTarget {
    /// Hidden: let WebView2 trim what it can.
    Low,
    /// Visible: restore normal behaviour before the user sees the window.
    Normal,
}

impl MemoryTarget {
    fn level(self) -> COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL {
        match self {
            Self::Low => COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW,
            Self::Normal => COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL,
        }
    }
}

/// Request `target` on the controller's `CoreWebView2`.
///
/// Returns `Ok(false)` when the runtime predates `ICoreWebView2_19` rather than
/// treating it as a failure: an older WebView2 simply cannot do this, and it is
/// not worth surfacing to the user. Every other COM failure is reported so the
/// caller can log it.
pub(crate) fn set_memory_target(
    controller: &ICoreWebView2Controller,
    target: MemoryTarget,
) -> Result<bool, String> {
    // SAFETY: `controller` is a live COM interface owned by the caller for the
    // duration of this call. `CoreWebView2` and `SetMemoryUsageTargetLevel` are
    // both in-process calls that only borrow it.
    unsafe {
        let core = controller
            .CoreWebView2()
            .map_err(|e| format!("resolve CoreWebView2: {e}"))?;
        let versioned = match core.cast::<ICoreWebView2_19>() {
            Ok(versioned) => versioned,
            // Only "this runtime does not implement the interface" is expected
            // and silent. Any other COM failure is a real fault and must be
            // reported rather than disguised as an old runtime.
            Err(error) if error.code() == E_NOINTERFACE => return Ok(false),
            Err(error) => return Err(format!("cast to ICoreWebView2_19: {error}")),
        };
        versioned
            .SetMemoryUsageTargetLevel(target.level())
            .map_err(|e| format!("set memory usage target level: {e}"))?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_and_visible_map_to_distinct_target_levels() {
        assert_eq!(
            MemoryTarget::Low.level(),
            COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW
        );
        assert_eq!(
            MemoryTarget::Normal.level(),
            COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL
        );
        assert_ne!(MemoryTarget::Low.level(), MemoryTarget::Normal.level());
    }
}
