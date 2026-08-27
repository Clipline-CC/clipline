//! Shared image helpers: PNG encoding and crash-safe sibling-temp publishing.
//! Any feature that writes a media file beside user content (posters,
//! screenshots) publishes through [SiblingTemp] so a crash never leaves a
//! half-written file at the destination.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Encode tightly-packed RGBA pixels as a PNG with default compression.
pub fn encode_rgba_png(width: u32, height: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    encode_rgba_png_with(width, height, rgba, png::Compression::Default)
}

/// Encode tightly-packed RGBA pixels as a PNG. Full-resolution screenshot
/// frames are ~30 MB of RGBA; default zlib settings cost seconds per shot,
/// so the caller picks the speed/size tradeoff.
pub fn encode_rgba_png_with(
    width: u32,
    height: u32,
    rgba: &[u8],
    compression: png::Compression,
) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        // One fixed filter avoids the per-row search; Sub is the crate
        // default and cheap on screen content.
        encoder.set_filter(png::FilterType::Sub);
        encoder.set_compression(compression);
        let mut writer = encoder.write_header().ok()?;
        writer.write_image_data(rgba).ok()?;
    }
    Some(out)
}

static NEXT_SIBLING_TEMP_ID: AtomicU64 = AtomicU64::new(0);

/// A temp file created next to its destination so publishing is an atomic
/// same-volume rename. Dropping an unpublished temp deletes it; publishing
/// consumes it.
#[derive(Debug)]
pub(crate) struct SiblingTemp {
    path: PathBuf,
    file: Option<File>,
    armed: bool,
}

impl SiblingTemp {
    pub fn reserve(destination: &Path) -> Result<Self, String> {
        let file_name = destination
            .file_name()
            .ok_or_else(|| "destination path has no file name".to_string())?;
        for _ in 0..64 {
            let id = NEXT_SIBLING_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let mut temp_name = file_name.to_os_string();
            temp_name.push(format!(".tmp.{}.{id}", std::process::id()));
            let path = destination.with_file_name(temp_name);
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file: Some(file),
                        armed: true,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(format!("reserve sibling temp: {error}")),
            }
        }
        Err("reserve sibling temp: unique-name attempts exhausted".to_string())
    }

    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn write_all(&mut self, bytes: &[u8]) -> Result<(), String> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| "sibling temp is already closed".to_string())?;
        file.write_all(bytes)
            .and_then(|()| file.flush())
            .map_err(|error| format!("write sibling temp: {error}"))
    }

    pub fn publish(mut self, destination: &Path) -> Result<(), String> {
        // Close the handle before the atomic rename; publishing must not
        // depend on the platform's sharing flags for an open Rust File.
        drop(self.file.take());
        atomic_replace_file(&self.path, destination)
            .map_err(|error| format!("finalize sibling temp: {error}"))?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for SiblingTemp {
    fn drop(&mut self) {
        drop(self.file.take());
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(windows)]
fn atomic_replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    crate::windows::replace_file(from, to)
}

#[cfg(not(windows))]
fn atomic_replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_rgba_png_round_trips_a_known_bitmap() {
        // 2x2: red, green / blue, white.
        let rgba = [
            255, 0, 0, 255, //
            0, 255, 0, 255, //
            0, 0, 255, 255, //
            255, 255, 255, 255,
        ];
        let png_bytes = encode_rgba_png(2, 2, &rgba).expect("encode");

        let decoder = png::Decoder::new(std::io::Cursor::new(&png_bytes));
        let mut reader = decoder.read_info().expect("header");
        let mut decoded = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut decoded).expect("frame");

        assert_eq!(info.width, 2);
        assert_eq!(info.height, 2);
        assert_eq!(info.color_type, png::ColorType::Rgba);
        assert_eq!(&decoded[..info.buffer_size()], &rgba);
    }

    #[test]
    fn sibling_temp_atomically_replaces_stale_destination() {
        let dir = test_dir("atomic-publish");
        let dest = dir.join("shot.png");
        std::fs::write(&dest, b"stale").unwrap();
        let mut temp = SiblingTemp::reserve(&dest).unwrap();
        let temp_path = temp.path().to_path_buf();
        temp.write_all(b"complete png").unwrap();

        temp.publish(&dest).unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), b"complete png");
        assert!(!temp_path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn concurrent_sibling_temps_have_independent_owned_paths() {
        let dir = test_dir("owned-temp");
        let dest = dir.join("shot.png");
        let first = SiblingTemp::reserve(&dest).unwrap();
        let second = SiblingTemp::reserve(&dest).unwrap();
        let first_path = first.path().to_path_buf();
        let second_path = second.path().to_path_buf();

        assert_ne!(first_path, second_path);
        assert_eq!(first_path.parent(), dest.parent());
        assert_eq!(second_path.parent(), dest.parent());
        assert!(first_path.exists());
        assert!(second_path.exists());

        drop(first);
        assert!(!first_path.exists(), "unpublished temp must clean up on drop");
        assert!(second_path.exists());
        drop(second);
        assert!(!second_path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reserve_rejects_a_destination_without_a_file_name() {
        let error = SiblingTemp::reserve(Path::new("C:\\")).unwrap_err();
        assert!(error.contains("no file name"));
    }

    fn test_dir(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "clipline-image-{label}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
