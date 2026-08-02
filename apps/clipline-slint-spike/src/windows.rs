use clipline_playback::windows::{VideoHostError, WindowsVideoHost, WindowsVideoTarget};
use clipline_playback::{LogicalVideoRect, PresentationState, ScaleFactor};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::ComponentHandle;

use crate::CliplineSpike;

pub fn attach_video_host(
    window: &slint::Window,
) -> Result<(WindowsVideoHost, WindowsVideoTarget), String> {
    let window_handle = window.window_handle();
    let handle = window_handle
        .window_handle()
        .map_err(|error| format!("Slint Win32 handle is unavailable: {error}"))?;
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return Err("Slint presentation spike requires a Win32 window handle".to_owned());
    };
    let mut host = WindowsVideoHost::attach(handle.hwnd.get()).map_err(video_host_error)?;
    let target = host.take_target().map_err(video_host_error)?;
    Ok((host, target))
}

pub fn update_video_host(host: &mut WindowsVideoHost, ui: &CliplineSpike) -> Result<(), String> {
    let logical = LogicalVideoRect::new(
        ui.get_video_stage_x(),
        ui.get_video_stage_y(),
        ui.get_video_stage_width(),
        ui.get_video_stage_height(),
    )
    .map_err(|error| format!("invalid Slint video geometry: {error}"))?;
    let scale = ScaleFactor::new(ui.window().scale_factor())
        .map_err(|error| format!("invalid Slint scale factor: {error}"))?;
    let physical = logical
        .to_physical(scale)
        .map_err(|error| format!("invalid physical video geometry: {error}"))?;
    let state = if !ui.window().is_visible() {
        PresentationState::Minimized
    } else if ui.get_review_visible() && ui.window().is_visible() {
        PresentationState::Visible
    } else {
        PresentationState::Occluded
    };
    host.update(physical, state)
        .map(|_| ())
        .map_err(video_host_error)
}

pub fn occlude_video_host(host: &mut WindowsVideoHost) -> Result<(), String> {
    let bounds = clipline_playback::PhysicalVideoRect::new(0, 0, 0, 0);
    host.update(bounds, PresentationState::Occluded)
        .map(|_| ())
        .map_err(video_host_error)
}

fn video_host_error(error: VideoHostError) -> String {
    error.to_string()
}
