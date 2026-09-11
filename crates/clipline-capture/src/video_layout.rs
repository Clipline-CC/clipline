//! Aspect-preserving placement in a fixed NV12 output. No platform objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VideoRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

pub(crate) fn fitted_video_rect(
    source_width: u32,
    source_height: u32,
    output_width: u32,
    output_height: u32,
) -> Result<VideoRect, &'static str> {
    if source_width == 0
        || source_height == 0
        || output_width < 2
        || output_height < 2
        || !output_width.is_multiple_of(2)
        || !output_height.is_multiple_of(2)
        || output_width > i32::MAX as u32
        || output_height > i32::MAX as u32
    {
        return Err("invalid video dimensions");
    }
    let (width, height) = if u64::from(source_width) * u64::from(output_height)
        > u64::from(source_height) * u64::from(output_width)
    {
        (
            output_width,
            (u64::from(source_height) * u64::from(output_width) / u64::from(source_width)) as u32,
        )
    } else {
        (
            (u64::from(source_width) * u64::from(output_height) / u64::from(source_height)) as u32,
            output_height,
        )
    };
    let (width, height) = (width & !1, height & !1);
    if width < 2 || height < 2 {
        return Err("fitted video is smaller than one NV12 chroma sample");
    }
    Ok(VideoRect {
        x: ((output_width - width) / 2) & !1,
        y: ((output_height - height) / 2) & !1,
        width,
        height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fullscreen_resolution_change_preserves_shape_and_centers_content() {
        assert_eq!(
            fitted_video_rect(1280, 720, 1280, 720).unwrap(),
            VideoRect {
                x: 0,
                y: 0,
                width: 1280,
                height: 720
            }
        );
        assert_eq!(
            fitted_video_rect(720, 480, 1280, 720).unwrap(),
            VideoRect {
                x: 100,
                y: 0,
                width: 1080,
                height: 720
            }
        );
        assert_eq!(
            fitted_video_rect(1920, 1080, 640, 480).unwrap(),
            VideoRect {
                x: 0,
                y: 60,
                width: 640,
                height: 360
            }
        );
        assert_eq!(
            fitted_video_rect(480, 720, 1280, 720).unwrap(),
            VideoRect {
                x: 400,
                y: 0,
                width: 480,
                height: 720
            }
        );
    }

    #[test]
    fn fitted_rect_stays_inside_output_and_aligns_nv12_chroma() {
        for (sw, sh, ow, oh) in [
            (853, 479, 1280, 720),
            (853, 477, 1280, 720), // Width-limited fit has odd height 715 before masking.
            (1001, 777, 640, 360),
            (3840, 2160, 854, 480),
            (u32::MAX, u32::MAX, 1280, 720),
        ] {
            let rect = fitted_video_rect(sw, sh, ow, oh).unwrap();
            assert!(rect.x + rect.width <= ow && rect.y + rect.height <= oh);
            assert!(rect.width >= 2 && rect.height >= 2);
            assert!(
                [rect.x, rect.y, rect.width, rect.height]
                    .iter()
                    .all(|v| v % 2 == 0)
            );
            // Rounding either fitted side down costs less than two pixels.
            let expected = (ow as f64 / sw as f64).min(oh as f64 / sh as f64);
            assert!((rect.width as f64 - sw as f64 * expected).abs() < 2.0);
            assert!((rect.height as f64 - sh as f64 * expected).abs() < 2.0);
        }
    }

    #[test]
    fn invalid_or_unrepresentable_dimensions_fail_instead_of_distorting() {
        for dims in [
            (0, 480, 1280, 720),
            (720, 0, 1280, 720),
            (720, 480, 1279, 720),
            (720, 480, 1280, 1),
            (u32::MAX, 1, 2, 2),
            (2, 2, u32::MAX - 1, 2),
        ] {
            assert!(fitted_video_rect(dims.0, dims.1, dims.2, dims.3).is_err());
        }
    }
}
