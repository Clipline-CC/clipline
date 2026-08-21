//! Neutral still-image core for screenshot capture.
//!
//! A screenshot is orchestration over existing primitives: a WGC grab is read
//! back as row-pitched BGRA (`windows/nv12.rs`), cropped on the CPU, and
//! encoded elsewhere. This module holds the platform-neutral part: the packed
//! image type, selection clamping/cropping, BGRA-to-RGBA conversion, and the
//! virtual-desktop union math. No `unsafe`, no Windows types.

/// Rectangle with a possibly-negative origin: drag selections and monitor
/// positions in virtual-desktop coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlacedRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Tightly packed BGRA image (`stride == width * 4`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BgraImage {
    width: u32,
    height: u32,
    bytes: Vec<u8>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StillError {
    #[error("image dimensions must be non-zero")]
    InvalidDimensions,
    #[error("BGRA row pitch is smaller than the image row")]
    InvalidStride,
    #[error("BGRA buffer is shorter than the declared dimensions and row pitch")]
    BufferTooSmall,
    #[error("selection is empty or entirely outside the image")]
    InvalidSelection,
}

impl BgraImage {
    /// Packs a row-pitched GPU readback into a tight image.
    pub fn from_readback(
        width: u32,
        height: u32,
        stride: usize,
        bgra: &[u8],
    ) -> Result<Self, StillError> {
        if width == 0 || height == 0 {
            return Err(StillError::InvalidDimensions);
        }
        let row_bytes = width as usize * 4;
        if stride < row_bytes {
            return Err(StillError::InvalidStride);
        }
        let rows = height as usize;
        let required = (rows - 1)
            .checked_mul(stride)
            .and_then(|prefix| prefix.checked_add(row_bytes))
            .ok_or(StillError::BufferTooSmall)?;
        if bgra.len() < required {
            return Err(StillError::BufferTooSmall);
        }
        let mut bytes = Vec::with_capacity(rows * row_bytes);
        for row in 0..rows {
            let start = row * stride;
            bytes.extend_from_slice(&bgra[start..start + row_bytes]);
        }
        Ok(Self {
            width,
            height,
            bytes,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Copies the selected region into a new tight image. The selection is
    /// clamped to the image first; empty or fully-outside selections are an
    /// error, never a panic.
    pub fn crop(&self, selection: PlacedRect) -> Result<BgraImage, StillError> {
        let clipped =
            clip_to_image(self.width, self.height, selection).ok_or(StillError::InvalidSelection)?;
        let src_stride = self.width as usize * 4;
        let crop_width = clipped.width as usize;
        let mut bytes = Vec::with_capacity(crop_width * clipped.height as usize * 4);
        for row in 0..clipped.height as usize {
            let start = (clipped.y as usize + row) * src_stride + clipped.x as usize * 4;
            bytes.extend_from_slice(&self.bytes[start..start + crop_width * 4]);
        }
        Ok(BgraImage {
            width: clipped.width,
            height: clipped.height,
            bytes,
        })
    }

    /// Byte-exact BGRA to RGBA swap (alpha preserved).
    pub fn to_rgba(&self) -> Vec<u8> {
        self.bytes
            .chunks_exact(4)
            .flat_map(|px| [px[2], px[1], px[0], px[3]])
            .collect()
    }
}

/// Clamps `selection` to the image bounds. Returns `None` when the clipped
/// area is empty: zero-area or fully outside.
pub fn clip_to_image(width: u32, height: u32, selection: PlacedRect) -> Option<PlacedRect> {
    if selection.width == 0 || selection.height == 0 {
        return None;
    }
    let left = i64::from(selection.x).clamp(0, i64::from(width));
    let top = i64::from(selection.y).clamp(0, i64::from(height));
    let right = (i64::from(selection.x) + i64::from(selection.width)).min(i64::from(width));
    let bottom = (i64::from(selection.y) + i64::from(selection.height)).min(i64::from(height));
    if right <= left || bottom <= top {
        return None;
    }
    Some(PlacedRect {
        x: left as i32,
        y: top as i32,
        width: (right - left) as u32,
        height: (bottom - top) as u32,
    })
}

/// Bounding box of all monitors in virtual-desktop coordinates. The origin is
/// non-zero whenever any monitor sits left of or above the primary.
pub fn virtual_desktop_union(monitors: &[PlacedRect]) -> Option<PlacedRect> {
    monitors
        .iter()
        .filter(|m| m.width > 0 && m.height > 0)
        .map(|m| {
            (
                i64::from(m.x),
                i64::from(m.y),
                i64::from(m.x) + i64::from(m.width),
                i64::from(m.y) + i64::from(m.height),
            )
        })
        .reduce(|a, b| (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)))
        .map(|(left, top, right, bottom)| PlacedRect {
            x: left as i32,
            y: top as i32,
            width: (right - left) as u32,
            height: (bottom - top) as u32,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tight(bytes: &[u8], width: u32, height: u32) -> BgraImage {
        BgraImage::from_readback(width, height, width as usize * 4, bytes).unwrap()
    }

    #[test]
    fn readback_packing_honours_row_pitch() {
        // 2x2 image with one padding byte per row; padding must not leak in.
        let mut pitched = Vec::new();
        pitched.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 0xEE]);
        pitched.extend_from_slice(&[9, 10, 11, 12, 13, 14, 15, 16, 0xDD]);

        let image = BgraImage::from_readback(2, 2, 9, &pitched).unwrap();

        assert_eq!(image.width(), 2);
        assert_eq!(image.height(), 2);
        assert_eq!(
            image.bytes(),
            &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
        );
    }

    #[test]
    fn readback_rejects_bad_stride_and_short_buffer() {
        assert_eq!(
            BgraImage::from_readback(2, 2, 7, &[0; 8]),
            Err(StillError::InvalidStride)
        );
        assert_eq!(
            BgraImage::from_readback(2, 2, 8, &[0; 15]),
            Err(StillError::BufferTooSmall)
        );
        assert_eq!(
            BgraImage::from_readback(0, 2, 0, &[]),
            Err(StillError::InvalidDimensions)
        );
    }

    #[test]
    fn clip_clamps_partial_selections_to_image_bounds() {
        let clipped = clip_to_image(
            100,
            80,
            PlacedRect {
                x: -10,
                y: -5,
                width: 200,
                height: 200,
            },
        )
        .unwrap();
        assert_eq!(
            clipped,
            PlacedRect {
                x: 0,
                y: 0,
                width: 100,
                height: 80
            }
        );

        let bottom_right = clip_to_image(
            100,
            80,
            PlacedRect {
                x: 90,
                y: 70,
                width: 50,
                height: 50,
            },
        )
        .unwrap();
        assert_eq!(
            bottom_right,
            PlacedRect {
                x: 90,
                y: 70,
                width: 10,
                height: 10
            }
        );
    }

    #[test]
    fn empty_or_outside_selections_are_rejected_without_panicking() {
        let zero_area = clip_to_image(
            100,
            80,
            PlacedRect {
                x: 10,
                y: 10,
                width: 0,
                height: 0,
            },
        );
        assert_eq!(zero_area, None);

        let outside = clip_to_image(
            100,
            80,
            PlacedRect {
                x: 150,
                y: 10,
                width: 20,
                height: 20,
            },
        );
        assert_eq!(outside, None);

        let image = tight(&[0; 16], 2, 2);
        assert_eq!(
            image.crop(PlacedRect {
                x: 150,
                y: 10,
                width: 20,
                height: 20
            }),
            Err(StillError::InvalidSelection)
        );
    }

    #[test]
    fn crop_copies_only_the_selected_pixels_from_a_pitched_source() {
        // Row 0: BGRA blue, green, red + pad. Row 1: white, gray, black + pad.
        let mut pitched = Vec::new();
        pitched.extend_from_slice(&[255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 0xEE]);
        pitched.extend_from_slice(&[255, 255, 255, 255, 128, 128, 128, 255, 0, 0, 0, 255, 0xDD]);

        let image = BgraImage::from_readback(3, 2, 13, &pitched).unwrap();

        let cropped = image
            .crop(PlacedRect {
                x: 2,
                y: 0,
                width: 1,
                height: 2,
            })
            .unwrap();

        assert_eq!(cropped.width(), 1);
        assert_eq!(cropped.height(), 2);
        assert_eq!(cropped.bytes(), &[0, 0, 255, 255, 0, 0, 0, 255]);
    }

    #[test]
    fn bgra_to_rgba_conversion_is_byte_exact() {
        let image = tight(
            &[0, 0, 255, 255, 255, 0, 0, 255, 0, 255, 0, 128],
            3,
            1,
        );

        assert_eq!(
            image.to_rgba(),
            vec![255, 0, 0, 255, 0, 0, 255, 255, 0, 255, 0, 128]
        );
    }

    #[test]
    fn union_computes_nonzero_origin_for_monitors_left_of_or_above_primary() {
        let primary = PlacedRect {
            x: 1920,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let left = PlacedRect {
            x: -2560,
            y: 0,
            width: 2560,
            height: 1440,
        };

        let union = virtual_desktop_union(&[primary, left]).unwrap();
        assert_eq!(
            union,
            PlacedRect {
                x: -2560,
                y: 0,
                width: 6400,
                height: 1440
            }
        );

        let above = PlacedRect {
            x: 0,
            y: -1440,
            width: 1920,
            height: 1440,
        };
        let union = virtual_desktop_union(&[primary, above]).unwrap();
        assert_eq!(
            union,
            PlacedRect {
                x: 0,
                y: -1440,
                width: 3840,
                height: 2520
            }
        );

        assert_eq!(virtual_desktop_union(&[]), None);
    }
}
