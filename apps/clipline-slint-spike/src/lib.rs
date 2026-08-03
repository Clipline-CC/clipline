//! Non-distributed Slint presentation spike for Clipline.

pub mod catalog;
pub mod cloud;
pub mod cloud_thumbnail;
pub mod controller;
pub mod cpu_frame;
pub mod desktop;
pub mod model;
pub mod options;
pub mod poster;
pub mod settings;
pub mod shell;

#[cfg(windows)]
pub mod live;
#[cfg(windows)]
pub mod windows;

slint::include_modules!();

pub fn create_window() -> Result<CliplineSpike, slint::PlatformError> {
    let window = CliplineSpike::new()?;
    // The production component starts empty. Catalog rows are published only
    // from the long-lived, token-fenced Rust controller after window attach.
    window.set_library_items(slint::ModelRc::new(slint::VecModel::from(
        Vec::<LibraryItem>::new(),
    )));

    let timeline_markers = vec![
        TimelineMarker {
            position: 0.08,
            label: "Round start".into(),
            marker_color: color(100, 164, 214),
        },
        TimelineMarker {
            position: 0.24,
            label: "Combo".into(),
            marker_color: color(217, 150, 42),
        },
        TimelineMarker {
            position: 0.47,
            label: "Bookmark".into(),
            marker_color: color(233, 196, 83),
        },
        TimelineMarker {
            position: 0.68,
            label: "Elimination".into(),
            marker_color: color(208, 82, 75),
        },
        TimelineMarker {
            position: 0.88,
            label: "Round end".into(),
            marker_color: color(116, 185, 126),
        },
    ];
    window.set_timeline_markers(slint::ModelRc::new(slint::VecModel::from(timeline_markers)));
    Ok(window)
}

fn color(red: u8, green: u8, blue: u8) -> slint::Color {
    slint::Color::from_rgb_u8(red, green, blue)
}
