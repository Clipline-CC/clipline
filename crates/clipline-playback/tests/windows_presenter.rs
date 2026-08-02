#![cfg(windows)]

use clipline_playback::windows::{
    classify_present_result, validate_video_bounds, PresentOutcome, VideoHostError,
    WindowsVideoHost,
};
use clipline_playback::PhysicalVideoRect;
use windows::Win32::Foundation::DXGI_STATUS_OCCLUDED;
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_DEVICE_HUNG, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
    DXGI_ERROR_DRIVER_INTERNAL_ERROR, DXGI_ERROR_WAS_STILL_DRAWING,
};

#[test]
fn parent_and_bounds_validation_fail_closed() {
    assert_eq!(
        WindowsVideoHost::attach(0).expect_err("null parent must fail"),
        VideoHostError::NullParent
    );
    assert_eq!(
        WindowsVideoHost::attach(1).expect_err("fabricated parent must fail"),
        VideoHostError::InvalidParent
    );

    assert_eq!(
        validate_video_bounds(PhysicalVideoRect::new(-1, 0, 640, 360)),
        Err(VideoHostError::InvalidBounds)
    );
    assert_eq!(
        validate_video_bounds(PhysicalVideoRect::new(0, 0, i32::MAX as u32 + 1, 360)),
        Err(VideoHostError::InvalidBounds)
    );
    validate_video_bounds(PhysicalVideoRect::new(0, 0, 0, 0))
        .expect("zero bounds are valid hidden geometry");
}

#[test]
fn present_hresult_classification_is_recovery_aware() {
    assert_eq!(classify_present_result(0), PresentOutcome::Presented);
    assert_eq!(
        classify_present_result(DXGI_STATUS_OCCLUDED.0),
        PresentOutcome::Occluded
    );
    assert_eq!(
        classify_present_result(DXGI_ERROR_WAS_STILL_DRAWING.0),
        PresentOutcome::Backpressured
    );
    for code in [
        DXGI_ERROR_DEVICE_REMOVED.0,
        DXGI_ERROR_DEVICE_RESET.0,
        DXGI_ERROR_DEVICE_HUNG.0,
        DXGI_ERROR_DRIVER_INTERNAL_ERROR.0,
    ] {
        assert_eq!(classify_present_result(code), PresentOutcome::DeviceLost);
    }
    assert_eq!(classify_present_result(-1), PresentOutcome::Failed);
}
