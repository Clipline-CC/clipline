//! Non-distributed Slint presentation spike for Clipline.

pub mod controller;
pub mod cpu_frame;
pub mod model;
pub mod options;

#[cfg(windows)]
pub mod live;
#[cfg(windows)]
pub mod windows;

slint::include_modules!();

pub fn create_window() -> Result<CliplineSpike, slint::PlatformError> {
    use model::ClipKind;

    let window = CliplineSpike::new()?;
    let library_items: Vec<LibraryItem> = model::representative_library()
        .into_iter()
        .map(|row| {
            let (kind, kind_color) = match row.kind {
                ClipKind::Replay => ("REPLAY", color(217, 150, 42)),
                ClipKind::Session => ("SESSION", color(100, 164, 214)),
                ClipKind::Trim => ("TRIM", color(116, 185, 126)),
            };
            let seed = u16::from(row.poster_seed);
            LibraryItem {
                title: row.title.into(),
                subtitle: row.subtitle.into(),
                duration: model::format_clock(row.duration_ticks).into(),
                kind: kind.into(),
                kind_color,
                poster_a: color(
                    (34 + seed * 7 % 72) as u8,
                    (23 + seed * 11 % 58) as u8,
                    (16 + seed * 13 % 46) as u8,
                ),
                poster_b: color(
                    (92 + seed * 5 % 92) as u8,
                    (54 + seed * 3 % 74) as u8,
                    (28 + seed * 9 % 64) as u8,
                ),
            }
        })
        .collect();
    window.set_library_items(slint::ModelRc::new(slint::VecModel::from(library_items)));

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
