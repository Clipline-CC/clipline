//! CPU BGRA -> NV12 conversion for systems without a D3D11 video processor.
//!
//! WGC still supplies a GPU texture on software-only Windows VMs. The Windows
//! wrapper reads that texture back as row-pitched BGRA; this neutral module
//! performs the fixed crop/scale and the same full-range RGB Rec.709 to
//! limited-range YCbCr Rec.709 conversion advertised by the hardware path.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuCropRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CpuVideoError {
    #[error("input and output dimensions must be non-zero, with even output dimensions")]
    InvalidDimensions,
    #[error("crop rectangle is empty or outside the input frame")]
    InvalidCrop,
    #[error("BGRA row pitch is smaller than the input row")]
    InvalidStride,
    #[error("BGRA buffer is shorter than the declared dimensions and row pitch")]
    BufferTooSmall,
    #[error("video dimensions overflow the addressable buffer size")]
    SizeOverflow,
}

#[derive(Debug, Clone)]
pub struct CpuVideoConverter {
    input_width: u32,
    input_height: u32,
    source: CpuCropRect,
    output_width: u32,
    output_height: u32,
    destination: crate::video_layout::VideoRect,
}

impl CpuVideoConverter {
    pub fn new(
        input_width: u32,
        input_height: u32,
        crop: Option<CpuCropRect>,
        output_width: u32,
        output_height: u32,
    ) -> Result<Self, CpuVideoError> {
        if input_width == 0
            || input_height == 0
            || output_width == 0
            || output_height == 0
            || !output_width.is_multiple_of(2)
            || !output_height.is_multiple_of(2)
        {
            return Err(CpuVideoError::InvalidDimensions);
        }
        let source = crop.unwrap_or(CpuCropRect {
            x: 0,
            y: 0,
            width: input_width,
            height: input_height,
        });
        let right = source
            .x
            .checked_add(source.width)
            .ok_or(CpuVideoError::InvalidCrop)?;
        let bottom = source
            .y
            .checked_add(source.height)
            .ok_or(CpuVideoError::InvalidCrop)?;
        if source.width == 0 || source.height == 0 || right > input_width || bottom > input_height {
            return Err(CpuVideoError::InvalidCrop);
        }
        Ok(Self {
            input_width,
            input_height,
            source,
            output_width,
            output_height,
            destination: crate::video_layout::fitted_video_rect(
                source.width,
                source.height,
                output_width,
                output_height,
            )
            .map_err(|_| CpuVideoError::InvalidDimensions)?,
        })
    }

    pub fn convert(&self, bgra: &[u8], stride: usize) -> Result<Vec<u8>, CpuVideoError> {
        let input_row_bytes = usize::try_from(self.input_width)
            .ok()
            .and_then(|width| width.checked_mul(4))
            .ok_or(CpuVideoError::SizeOverflow)?;
        if stride < input_row_bytes {
            return Err(CpuVideoError::InvalidStride);
        }
        let input_height = self.input_height as usize;
        let required = input_height
            .checked_sub(1)
            .and_then(|rows| rows.checked_mul(stride))
            .and_then(|prefix| prefix.checked_add(input_row_bytes))
            .ok_or(CpuVideoError::SizeOverflow)?;
        if bgra.len() < required {
            return Err(CpuVideoError::BufferTooSmall);
        }

        let out_w = self.output_width as usize;
        let out_h = self.output_height as usize;
        let y_len = out_w
            .checked_mul(out_h)
            .ok_or(CpuVideoError::SizeOverflow)?;
        let total_len = y_len
            .checked_add(y_len / 2)
            .ok_or(CpuVideoError::SizeOverflow)?;
        // Limited-range black outside the fitted content: Y=16, U=V=128.
        // Fitted bounds are even, so each chroma block is wholly inside content
        // or wholly background. Avoid a clipping branch on every pixel sample.
        let mut nv12 = vec![16u8; total_len];
        nv12[y_len..].fill(128);
        let dest = self.destination;
        let rows = dest.y as usize..(dest.y + dest.height) as usize;
        let columns = dest.x as usize..(dest.x + dest.width) as usize;

        for out_y in rows.clone() {
            for out_x in columns.clone() {
                let (b, g, r) = self.source_pixel(bgra, stride, out_x, out_y);
                nv12[out_y * out_w + out_x] = rec709_limited_y(r, g, b);
            }
        }

        for out_y in rows.step_by(2) {
            for out_x in columns.clone().step_by(2) {
                let mut r_sum = 0u32;
                let mut g_sum = 0u32;
                let mut b_sum = 0u32;
                for dy in 0..2 {
                    for dx in 0..2 {
                        let (b, g, r) = self.source_pixel(bgra, stride, out_x + dx, out_y + dy);
                        r_sum += u32::from(r);
                        g_sum += u32::from(g);
                        b_sum += u32::from(b);
                    }
                }
                let r = ((r_sum + 2) / 4) as u8;
                let g = ((g_sum + 2) / 4) as u8;
                let b = ((b_sum + 2) / 4) as u8;
                let (u, v) = rec709_limited_uv(r, g, b);
                let uv = y_len + (out_y / 2) * out_w + out_x;
                nv12[uv] = u;
                nv12[uv + 1] = v;
            }
        }
        Ok(nv12)
    }

    fn source_pixel(&self, bgra: &[u8], stride: usize, out_x: usize, out_y: usize) -> (u8, u8, u8) {
        let dest = self.destination;
        let source_x = self.source.x as usize
            + (out_x - dest.x as usize) * self.source.width as usize / dest.width as usize;
        let source_y = self.source.y as usize
            + (out_y - dest.y as usize) * self.source.height as usize / dest.height as usize;
        let offset = source_y * stride + source_x * 4;
        (bgra[offset], bgra[offset + 1], bgra[offset + 2])
    }
}

// Fixed-point BT.709 studio-range coefficients at 16-bit precision. Each
// chroma row sums to zero so neutral gray remains exactly U=V=128.
fn rec709_limited_y(r: u8, g: u8, b: u8) -> u8 {
    let value =
        ((11_966i64 * i64::from(r) + 40_254i64 * i64::from(g) + 4_064i64 * i64::from(b) + 32_768)
            >> 16)
            + 16;
    value.clamp(16, 235) as u8
}

fn rec709_limited_uv(r: u8, g: u8, b: u8) -> (u8, u8) {
    let r = i64::from(r);
    let g = i64::from(g);
    let b = i64::from(b);
    let u = ((-6_596 * r - 22_189 * g + 28_785 * b + 32_768) >> 16) + 128;
    let v = ((28_785 * r - 26_145 * g - 2_640 * b + 32_768) >> 16) + 128;
    (u.clamp(16, 240) as u8, v.clamp(16, 240) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_letterbox_preserves_source_and_crop_shape() {
        for (width, height, crop, rect) in [
            (8, 4, None, (0, 2, 8, 4)),
            (4, 8, None, (2, 0, 4, 8)),
            (8, 8, None, (0, 0, 8, 8)),
            (
                16,
                8,
                Some(CpuCropRect {
                    x: 2,
                    y: 2,
                    width: 4,
                    height: 4,
                }),
                (0, 0, 8, 8),
            ),
        ] {
            let converter = CpuVideoConverter::new(width, height, crop, 8, 8).unwrap();
            let output = converter
                .convert(
                    &solid_bgra(width, height, 255, 255, 255),
                    width as usize * 4,
                )
                .unwrap();
            let (left, top, w, h) = rect;
            for y in 0..8 {
                for x in 0..8 {
                    let expected = if x >= left && x < left + w && y >= top && y < top + h {
                        235
                    } else {
                        16
                    };
                    assert_eq!(output[y * 8 + x], expected, "{width}x{height} at {x},{y}");
                }
            }
            assert!(output[64..].iter().all(|&v| v == 128));
        }
    }

    fn solid_bgra(width: u32, height: u32, b: u8, g: u8, r: u8) -> Vec<u8> {
        [b, g, r, 255]
            .into_iter()
            .cycle()
            .take(width as usize * height as usize * 4)
            .collect()
    }

    #[test]
    fn black_and_white_map_to_limited_range_nv12() {
        let converter = CpuVideoConverter::new(2, 2, None, 2, 2).unwrap();

        let black = converter.convert(&solid_bgra(2, 2, 0, 0, 0), 8).unwrap();
        assert_eq!(black, vec![16, 16, 16, 16, 128, 128]);

        let white = converter
            .convert(&solid_bgra(2, 2, 255, 255, 255), 8)
            .unwrap();
        assert_eq!(white, vec![235, 235, 235, 235, 128, 128]);
    }

    #[test]
    fn primary_colors_use_rec709_chroma() {
        let converter = CpuVideoConverter::new(2, 2, None, 2, 2).unwrap();
        let red = converter.convert(&solid_bgra(2, 2, 0, 0, 255), 8).unwrap();
        assert_eq!(red, vec![63, 63, 63, 63, 102, 240]);

        let blue = converter.convert(&solid_bgra(2, 2, 255, 0, 0), 8).unwrap();
        assert_eq!(blue, vec![32, 32, 32, 32, 240, 118]);
    }

    #[test]
    fn row_pitch_padding_is_not_treated_as_pixels() {
        let converter = CpuVideoConverter::new(2, 2, None, 2, 2).unwrap();
        let mut pitched = Vec::new();
        pitched.extend_from_slice(&[0, 0, 0, 255, 0, 0, 0, 255, 9, 9, 9, 9]);
        pitched.extend_from_slice(&[0, 0, 0, 255, 0, 0, 0, 255, 7, 7, 7, 7]);

        let nv12 = converter.convert(&pitched, 12).unwrap();

        assert_eq!(nv12, vec![16, 16, 16, 16, 128, 128]);
    }

    #[test]
    fn crop_and_scale_select_the_configured_source_rectangle() {
        // Four source columns: black, black, white, white. Cropping the right
        // half and scaling it to 2x2 must produce solid white.
        let mut bgra = Vec::new();
        for _ in 0..2 {
            bgra.extend_from_slice(&[0, 0, 0, 255, 0, 0, 0, 255]);
            bgra.extend_from_slice(&[255, 255, 255, 255, 255, 255, 255, 255]);
        }
        let converter = CpuVideoConverter::new(
            4,
            2,
            Some(CpuCropRect {
                x: 2,
                y: 0,
                width: 2,
                height: 2,
            }),
            2,
            2,
        )
        .unwrap();

        let nv12 = converter.convert(&bgra, 16).unwrap();

        assert_eq!(nv12, vec![235, 235, 235, 235, 128, 128]);
    }

    #[test]
    fn rejects_invalid_dimensions_crop_stride_and_buffer() {
        assert!(CpuVideoConverter::new(0, 2, None, 2, 2).is_err());
        assert!(CpuVideoConverter::new(2, 2, None, 3, 2).is_err());
        assert!(
            CpuVideoConverter::new(
                2,
                2,
                Some(CpuCropRect {
                    x: 1,
                    y: 0,
                    width: 2,
                    height: 2,
                }),
                2,
                2,
            )
            .is_err()
        );

        let converter = CpuVideoConverter::new(2, 2, None, 2, 2).unwrap();
        assert!(converter.convert(&[0; 16], 7).is_err());
        assert!(converter.convert(&[0; 15], 8).is_err());
    }
}
