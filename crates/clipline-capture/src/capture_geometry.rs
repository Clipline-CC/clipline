//! Geometry failures are source changes, not idle-screen timeouts. Callers must
//! not keep encoding an old texture when a saved region no longer fits.
use crate::CaptureError;

pub(crate) fn validate_fixed_capture_region(
    source_width: u32,
    source_height: u32,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
) -> Result<(), CaptureError> {
    if width >= 2
        && height >= 2
        && x.checked_add(width)
            .is_some_and(|right| right <= source_width)
        && y.checked_add(height)
            .is_some_and(|bottom| bottom <= source_height)
    {
        return Ok(());
    }
    Err(CaptureError::SourceChanged(format!(
        "selected region {width}x{height} at ({x}, {y}) no longer fits the {source_width}x{source_height} display; select the capture region again"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fullscreen_shrink_invalidates_fixed_region_without_idle_fallback() {
        assert!(validate_fixed_capture_region(1280, 720, 0, 0, 1280, 720).is_ok());
        let error = validate_fixed_capture_region(720, 480, 0, 0, 1280, 720).unwrap_err();
        assert!(matches!(error, CaptureError::SourceChanged(_)));
        assert!(!error.is_timeout());
        assert!(error.to_string().contains("720x480"));
        assert!(error.to_string().contains("1280x720"));
    }
    #[test]
    fn fitting_partial_regions_remain_fixed() {
        assert!(validate_fixed_capture_region(1280, 720, 20, 30, 200, 100).is_ok());
        assert!(validate_fixed_capture_region(720, 480, 20, 30, 200, 100).is_ok());
        assert!(validate_fixed_capture_region(100, 100, 20, 30, 200, 100).is_err());
    }
    #[test]
    fn invalid_or_overflowing_geometry_is_not_an_idle_frame() {
        for (x, y, w, h) in [
            (u32::MAX, 0, 2, 2),
            (0, u32::MAX, 2, 2),
            (0, 0, 0, 2),
            (0, 0, 2, 1),
        ] {
            let error = validate_fixed_capture_region(1280, 720, x, y, w, h).unwrap_err();
            assert!(!error.is_timeout());
        }
    }
}
