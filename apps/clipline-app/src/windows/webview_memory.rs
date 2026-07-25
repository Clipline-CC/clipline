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
        let Ok(versioned) = core.cast::<ICoreWebView2_19>() else {
            // Runtime older than the API — nothing to do.
            return Ok(false);
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
