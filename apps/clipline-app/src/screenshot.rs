//! Screenshot orchestration: one-shot grab, optional crop, PNG encode,
//! atomic publish into the media root. Runs on its own worker thread so a
//! screenshot works while the recorder is idle; results reach the UI through
//! Tauri events, mirroring the service event pump.

use std::path::{Path, PathBuf};

use clipline_capture::still::{BgraImage, PlacedRect};
use clipline_capture::windows::still as grab;

use crate::image::{encode_rgba_png, SiblingTemp};
use crate::library::is_reserved_windows_file_name;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenshotMode {
    Region,
    Screen,
    Window,
}

impl ScreenshotMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Region => "region",
            Self::Screen => "screen",
            Self::Window => "window",
        }
    }
}

/// shot_<epoch>.png - the plan-mandated name. The stem survives the same
/// reserved-name check clip names pass through, and the extension is fixed.
pub fn screenshot_file_name(epoch_s: i64) -> String {
    let name = format!("shot_{epoch_s}.png");
    assert!(
        !is_reserved_windows_file_name("shot"),
        "shot_ prefix must never collide with a reserved Windows name"
    );
    name
}

/// Grab the target for mode and return the raw BGRA frame. Region mode
/// currently publishes the full cursor monitor; the crop arrives with the
/// Task 12 region overlay.
pub fn capture_frame(mode: ScreenshotMode) -> Result<BgraImage, String> {
    match mode {
        ScreenshotMode::Region | ScreenshotMode::Screen => {
            let Some(monitor_id) = grab::cursor_monitor_id() else {
                return Err("no monitor found under the mouse cursor".into());
            };
            grab::grab_monitor(&monitor_id).map_err(|error| error.to_string())
        }
        ScreenshotMode::Window => {
            let Some(raw_hwnd) = crate::windows::foreground_window() else {
                return Err("no active window to capture".into());
            };
            if Some(raw_hwnd) == crate::windows::current_main_window() {
                return Err("Clipline itself is active; focus the window you want".into());
            }
            grab::grab_window(raw_hwnd).map_err(|error| error.to_string())
        }
    }
}

/// What one screenshot produced: the published file plus the exact pixels
/// that went into it (the clipboard needs them without re-decoding).
pub struct SavedScreenshot {
    pub path: PathBuf,
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Full pipeline for a completed frame: optional crop, PNG encode, atomic
/// publish beside the clips.
pub fn save_frame(
    media_root: &Path,
    mode: ScreenshotMode,
    image: &BgraImage,
    selection: Option<PlacedRect>,
) -> Result<SavedScreenshot, String> {
    let rgba = crop_to_rgba(image, selection)?;
    let (width, height) = match selection {
        Some(rect) => (rect.width, rect.height),
        None => (image.width(), image.height()),
    };
    let path = publish_screenshot(media_root, crate::util::unix_now_i64(), width, height, &rgba)
        .map_err(|error| format!("{} screenshot: {error}", mode.label()))?;
    Ok(SavedScreenshot {
        path,
        rgba,
        width,
        height,
    })
}

/// Canonicalize without the \\?\ verbatim prefix Windows adds, so two
/// paths derived from the same location actually compare equal.
fn normalize_path(path: &Path) -> PathBuf {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let text = canonical.as_os_str().to_string_lossy();
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{unc}"))
    } else if let Some(local) = text.strip_prefix(r"\\?\") {
        PathBuf::from(local)
    } else {
        canonical
    }
}

/// Publish an RGBA frame as shot_<epoch>.png inside media_root.
/// Refuses destinations outside the root and surfaces unwritable roots as
/// errors instead of silently dropping the shot (m21 rules).
pub fn publish_screenshot(
    media_root: &Path,
    epoch_s: i64,
    width: u32,
    height: u32,
    rgba: &[u8],
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(media_root)
        .map_err(|error| format!("create media folder {}: {error}", media_root.display()))?;
    let destination = media_root.join(screenshot_file_name(epoch_s));
    let normalized_root = normalize_path(media_root);
    if !normalize_path(&destination).starts_with(&normalized_root) {
        return Err(format!(
            "refusing to save a screenshot outside the clips directory: {}",
            destination.display()
        ));
    }
    let png = encode_rgba_png(width, height, rgba)
        .ok_or_else(|| "PNG encoding failed for the captured image".to_string())?;
    let mut temp = SiblingTemp::reserve(&destination)?;
    temp.write_all(&png)?;
    temp.publish(&destination)?;
    Ok(destination)
}

fn crop_to_rgba(image: &BgraImage, selection: Option<PlacedRect>) -> Result<Vec<u8>, String> {
    let cropped = match selection {
        Some(rect) => image.crop(rect).map_err(|e| e.to_string())?,
        None => image.clone(),
    };
    Ok(cropped.to_rgba())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(std::path::PathBuf);
    impl TestDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "clipline-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).expect("create test dir");
            Self(dir)
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn file_name_matches_plan_and_reserved_name_rules() {
        assert_eq!(screenshot_file_name(1_755_000_000), "shot_1755000000.png");
        // The generated stem is never a reserved device name.
        assert!(!is_reserved_windows_file_name(
            screenshot_file_name(42).trim_end_matches(".png")
        ));
    }

    #[test]
    fn publish_writes_a_decodable_png_inside_the_media_root() {
        let dir = TestDir::new("shot-publish");
        let rgba = vec![10u8, 20, 30, 255, 40, 50, 60, 255];
        let path = publish_screenshot(&dir.0, 777, 2, 1, &rgba).expect("publish");
        assert_eq!(path.parent(), Some(dir.0.as_path()));
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("shot_777.png")
        );
        let decoded = png::Decoder::new(std::fs::File::open(&path).expect("open"))
            .read_info()
            .expect("decode");
        assert_eq!(decoded.info().width, 2);
        assert_eq!(decoded.info().height, 1);
    }

    #[test]
    fn publish_surfaces_an_unwritable_root_as_an_error() {
        // A path under a *file* can never become a directory.
        let dir = TestDir::new("shot-unwritable");
        let blocker = dir.0.join("blocker");
        std::fs::write(&blocker, b"x").expect("write blocker");
        let root = blocker.join("media");
        let error = publish_screenshot(&root, 9, 1, 1, &[0, 0, 0, 255])
            .expect_err("unwritable root must be an actionable error");
        assert!(
            error.contains("not writable") || error.contains("create media folder"),
            "actionable error, got: {error}"
        );
    }

    #[test]
    fn crop_clamps_the_selection_before_conversion() {
        let image = BgraImage::from_readback(
            2,
            2,
            8,
            &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
        )
        .expect("image");
        let rgba = crop_to_rgba(
            &image,
            Some(PlacedRect {
                x: 1,
                y: 1,
                width: 99,
                height: 99,
            }),
        )
        .expect("crop");
        assert_eq!(rgba.len(), 4); // clamped to one pixel
        let full = crop_to_rgba(&image, None).expect("full");
        assert_eq!(full.len(), 16);
    }
}
