//! Capture selection shared by the live recorder and hardware-independent tests.

/// Auto and Wgc retain Windows Graphics Capture. Explicit Desktop Duplication
/// is a border-free display/region choice, never an isolated-window backend.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(windows, derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(windows, serde(rename_all = "snake_case"))]
pub enum CaptureBackend {
    #[default]
    Auto,
    Wgc,
    DesktopDuplication,
}

pub(crate) fn open_capture<T>(
    backend: CaptureBackend,
    is_window: bool,
    open_dxgi: impl FnOnce() -> Result<T, String>,
    open_wgc: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    match backend {
        CaptureBackend::DesktopDuplication => {
            if is_window {
                return Err("Desktop Duplication cannot capture a single window. Select a display or region and turn off automatic game switching in Settings to keep capture border-free.".into());
            }
            // The opener includes first-frame acquisition. Neither initialization
            // nor first-frame failure may switch to a bordered capture API.
            open_dxgi()
        }
        CaptureBackend::Auto | CaptureBackend::Wgc => open_wgc(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_dxgi_never_opens_wgc_after_initialization_or_first_frame_failure() {
        for error in [
            "DXGI initialization failed",
            "first frame timed out",
            "capture ended",
        ] {
            let result = open_capture::<()>(
                CaptureBackend::DesktopDuplication,
                false,
                || Err(error.into()),
                || panic!("explicit border-free capture must never start WGC"),
            );
            assert_eq!(result, Err(error.into()));
        }
    }

    #[test]
    fn explicit_dxgi_rejects_windows_before_opening_either_backend() {
        let error = open_capture::<()>(
            CaptureBackend::DesktopDuplication,
            true,
            || panic!("DXGI cannot isolate a window"),
            || panic!("window selection must not override the no-border choice"),
        )
        .unwrap_err();
        assert!(error.contains("display or region"));
    }

    #[test]
    fn explicit_dxgi_returns_its_capture() {
        assert_eq!(
            open_capture(
                CaptureBackend::DesktopDuplication,
                false,
                || Ok(42),
                || panic!("WGC")
            ),
            Ok(42),
        );
    }

    #[test]
    fn auto_and_wgc_preserve_success_and_errors_for_every_source() {
        for backend in [CaptureBackend::Auto, CaptureBackend::Wgc] {
            for is_window in [false, true] {
                for result in [Ok(42), Err("WGC unavailable".into())] {
                    assert_eq!(
                        open_capture(backend, is_window, || panic!("DXGI"), || result.clone()),
                        result,
                    );
                }
            }
        }
    }
}
