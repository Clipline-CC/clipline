//! Capture selection shared by the live recorder and hardware-independent tests.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(windows, derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(windows, serde(rename_all = "snake_case"))]
pub enum CaptureBackend {
    #[default]
    Auto,
    #[cfg_attr(windows, serde(alias = "wgc"))]
    Default,
    #[cfg_attr(windows, serde(alias = "desktop_duplication"))]
    Fallback,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CapturePlan {
    Wgc,
    Dxgi,
    FullscreenFallback,
}

impl CapturePlan {
    pub(crate) fn for_source(
        backend: CaptureBackend,
        is_windows_11_or_later: bool,
        is_window: bool,
    ) -> Self {
        match backend {
            CaptureBackend::Default => Self::Wgc,
            CaptureBackend::Auto if is_windows_11_or_later => Self::Wgc,
            CaptureBackend::Auto | CaptureBackend::Fallback if is_window => {
                Self::FullscreenFallback
            }
            CaptureBackend::Auto | CaptureBackend::Fallback => Self::Dxgi,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_routes_by_windows_generation_and_source() {
        assert_eq!(
            CapturePlan::for_source(CaptureBackend::Auto, true, true),
            CapturePlan::Wgc
        );
        assert_eq!(
            CapturePlan::for_source(CaptureBackend::Auto, true, false),
            CapturePlan::Wgc
        );
        assert_eq!(
            CapturePlan::for_source(CaptureBackend::Auto, false, true),
            CapturePlan::FullscreenFallback
        );
        assert_eq!(
            CapturePlan::for_source(CaptureBackend::Auto, false, false),
            CapturePlan::Dxgi
        );
    }

    #[test]
    fn explicit_choices_ignore_windows_generation() {
        for is_windows_11 in [false, true] {
            for is_window in [false, true] {
                assert_eq!(
                    CapturePlan::for_source(CaptureBackend::Default, is_windows_11, is_window),
                    CapturePlan::Wgc
                );
                assert_eq!(
                    CapturePlan::for_source(CaptureBackend::Fallback, is_windows_11, is_window),
                    if is_window {
                        CapturePlan::FullscreenFallback
                    } else {
                        CapturePlan::Dxgi
                    }
                );
            }
        }
    }
}
